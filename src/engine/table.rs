//! Committed state of one table: the primary-key index into the commit log
//! and, while the table fits the memory budget, a decoded copy of its rows
//! stored column by column. Each column is one contiguous array indexed by
//! slot, so a scan walks memory in order and a filter reads only its own
//! column.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::ops::{Range, RangeInclusive};

use super::expr::{holds, ColPred, PredValue};
use super::helper::{self, Helper};
use super::ivf::Ivf;
use super::sq8::{Sq8, Sq8Query};
use super::IndexOptions;
use super::topk::TopK;
use super::veccol::{VecCol, VecRef};
use crate::error::DbResult;
use crate::sql::ast::CmpOp;
use crate::storage::catalog::TableDef;
use crate::storage::log::{LogStore, OpView, RowLoc};
use crate::storage::row::decode_row;
use crate::types::{DataType, Value, MILLIONTHS_BYTES};
use crate::bail;

const NO_SLOT: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub loc: RowLoc,
    slot: u32,
}

pub enum ValRef<'a> {
    Int(i64),
    Str(&'a str),
    Vector(VecRef<'a>),
}

/// Read-only view of one row, either materialized values or a slot of the
/// row cache.
#[derive(Clone, Copy)]
pub struct RowView<'a> {
    pub pk: i64,
    src: Src<'a>,
}

#[derive(Clone, Copy)]
enum Src<'a> {
    Values(&'a [Value]),
    Cache(&'a RowCache, usize),
}

impl<'a> RowView<'a> {
    pub fn new(pk: i64, values: &'a [Value]) -> RowView<'a> {
        RowView { pk, src: Src::Values(values) }
    }

    #[inline]
    pub fn get(&self, col: usize) -> ValRef<'a> {
        match self.src {
            Src::Values(values) => match &values[col] {
                Value::Int(i) => ValRef::Int(*i),
                Value::Str(s) => ValRef::Str(s),
                Value::Vector(v) => ValRef::Vector(VecRef::F32(v)),
            },
            Src::Cache(cache, slot) => cache.get(slot, col),
        }
    }

    pub fn to_values(self) -> Vec<Value> {
        let n = match self.src {
            Src::Values(values) => values.len(),
            Src::Cache(cache, _) => cache.cols.len(),
        };
        (0..n)
            .map(|i| match self.get(i) {
                ValRef::Int(x) => Value::Int(x),
                ValRef::Str(s) => Value::Str(s.to_string()),
                ValRef::Vector(v) => Value::Vector(v.to_vec()),
            })
            .collect()
    }
}

/// Rows by slot. Freed slots are reused by later inserts.
struct RowCache {
    pks: Vec<i64>,
    live: Vec<bool>,
    cols: Vec<ColData>,
    free: Vec<u32>,
}

enum ColData {
    /// The primary key column, read from `RowCache::pks`.
    Pk,
    Int(Vec<i64>),
    Str(StrCol),
    Vector(VecCol),
}

/// Strings of one column, back to back in one buffer.
struct StrCol {
    /// (offset, length) of each slot's string in `arena`.
    spans: Vec<(usize, usize)>,
    arena: String,
    /// Bytes of `arena` that no slot refers to any more.
    garbage: usize,
    /// While the column has few distinct values: a code per slot, so `=` and
    /// `<>` against a constant compare integers instead of strings.
    dict: Option<Dict>,
}

/// Distinct values a column may have and keep its dictionary.
const DICT_MAX: usize = 1024;
/// Code of a free slot.
const NO_CODE: u32 = u32::MAX;
/// Code that no slot has: a constant missing from the dictionary.
const ABSENT: u32 = u32::MAX - 1;

#[derive(Default)]
struct Dict {
    codes: Vec<u32>,
    ids: HashMap<Box<str>, u32>,
    /// Bytes of the strings in `ids`.
    text: usize,
}

impl StrCol {
    fn new() -> StrCol {
        StrCol { spans: Vec::new(), arena: String::new(), garbage: 0, dict: Some(Dict::default()) }
    }

    #[inline]
    fn get(&self, slot: usize) -> &str {
        let (off, len) = self.spans[slot];
        &self.arena[off..off + len]
    }

    fn grow(&mut self) {
        self.spans.push((0, 0));
        if let Some(d) = &mut self.dict {
            d.codes.push(NO_CODE);
        }
    }

    fn set(&mut self, slot: usize, s: &str) {
        self.clear(slot);
        self.spans[slot] = (self.arena.len(), s.len());
        self.arena.push_str(s);
        if let Some(d) = &mut self.dict {
            let code = match d.ids.get(s) {
                Some(&c) => c,
                None if d.ids.len() < DICT_MAX => {
                    let c = d.ids.len() as u32;
                    d.ids.insert(s.into(), c);
                    d.text += s.len();
                    c
                }
                // Too many distinct values: strings are compared from now on.
                None => {
                    self.dict = None;
                    return;
                }
            };
            d.codes[slot] = code;
        }
    }

    /// Forgets the slot's string. Garbage is kept below half the arena (or
    /// 4 KiB), so the arena stays within about twice the live strings.
    fn clear(&mut self, slot: usize) {
        self.garbage += self.spans[slot].1;
        self.spans[slot] = (0, 0);
        if let Some(d) = &mut self.dict {
            d.codes[slot] = NO_CODE;
        }
        if self.garbage > 4096 && self.garbage * 2 > self.arena.len() {
            self.compact();
        }
    }

    /// The code `=` compares with for value `v`, if the column has a
    /// dictionary.
    fn code(&self, v: &str) -> Option<u32> {
        self.dict.as_ref().map(|d| d.ids.get(v).copied().unwrap_or(ABSENT))
    }

    /// Rewrites the arena with only the strings still referenced.
    fn compact(&mut self) {
        let mut arena = String::with_capacity(self.arena.len() - self.garbage);
        for span in &mut self.spans {
            let (off, len) = *span;
            *span = (arena.len(), len);
            arena.push_str(&self.arena[off..off + len]);
        }
        self.arena = arena;
        self.garbage = 0;
    }

    fn bytes(&self) -> usize {
        let dict = self.dict.as_ref().map_or(0, |d| {
            d.codes.capacity() * 4 + d.ids.capacity() * (size_of::<(Box<str>, u32)>() + 1) + d.text
        });
        self.spans.capacity() * size_of::<(usize, usize)>() + self.arena.capacity() + dict
    }
}

impl ColData {
    fn grow(&mut self) {
        match self {
            ColData::Pk => {}
            ColData::Int(v) => v.push(0),
            ColData::Str(s) => s.grow(),
            ColData::Vector(c) => c.grow(),
        }
    }

    fn set(&mut self, slot: usize, value: &Value) {
        match (self, value) {
            (ColData::Pk, _) => {}
            (ColData::Int(c), Value::Int(x)) => c[slot] = *x,
            (ColData::Str(c), Value::Str(s)) => c.set(slot, s),
            (ColData::Vector(c), Value::Vector(v)) if v.len() == c.dim() => c.set(slot, v),
            _ => debug_assert!(false, "row value does not match its column type"),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            ColData::Pk => 0,
            ColData::Int(v) => v.capacity() * 8,
            ColData::Str(s) => s.bytes(),
            ColData::Vector(c) => c.bytes(),
        }
    }
}

impl RowCache {
    fn new(def: &TableDef) -> RowCache {
        let cols = def
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| match c.ty {
                _ if i == def.pk => ColData::Pk,
                DataType::BigInt => ColData::Int(Vec::new()),
                DataType::Varchar(_) => ColData::Str(StrCol::new()),
                DataType::Vector(dim) => ColData::Vector(VecCol::new(dim as usize)),
            })
            .collect();
        RowCache { pks: Vec::new(), live: Vec::new(), cols, free: Vec::new() }
    }

    /// Bytes `n` rows take, not counting string contents, with vectors in
    /// codes: a column that turns to float32 is caught by `load_cache`
    /// checking as it goes.
    fn estimate(def: &TableDef, n: usize) -> usize {
        let per_row: usize = def
            .columns
            .iter()
            .map(|c| match c.ty {
                DataType::BigInt => 8,
                DataType::Varchar(_) => size_of::<(usize, usize)>() + 4,
                DataType::Vector(dim) => dim as usize * MILLIONTHS_BYTES,
            })
            .sum();
        n.saturating_mul(per_row + 9)
    }

    fn reserve(&mut self, n: usize) {
        self.pks.reserve_exact(n);
        self.live.reserve_exact(n);
        for c in &mut self.cols {
            match c {
                ColData::Pk => {}
                ColData::Int(v) => v.reserve_exact(n),
                ColData::Str(s) => {
                    s.spans.reserve_exact(n);
                    if let Some(d) = &mut s.dict {
                        d.codes.reserve_exact(n);
                    }
                }
                ColData::Vector(c) => c.reserve_exact(n),
            }
        }
    }

    fn insert(&mut self, pk: i64, row: &[Value]) -> u32 {
        let slot = match self.free.pop() {
            Some(s) => s as usize,
            None => {
                self.pks.push(0);
                self.live.push(false);
                for c in &mut self.cols {
                    c.grow();
                }
                self.pks.len() - 1
            }
        };
        self.pks[slot] = pk;
        self.live[slot] = true;
        for (c, v) in self.cols.iter_mut().zip(row) {
            c.set(slot, v);
        }
        slot as u32
    }

    fn remove(&mut self, slot: u32) {
        let s = slot as usize;
        if std::mem::replace(&mut self.live[s], false) {
            for c in &mut self.cols {
                if let ColData::Str(sc) = c {
                    sc.clear(s);
                }
            }
            self.free.push(slot);
        }
    }

    #[inline]
    fn get(&self, slot: usize, col: usize) -> ValRef<'_> {
        match &self.cols[col] {
            ColData::Pk => ValRef::Int(self.pks[slot]),
            ColData::Int(v) => ValRef::Int(v[slot]),
            ColData::Str(s) => ValRef::Str(s.get(slot)),
            ColData::Vector(c) => ValRef::Vector(c.get(slot)),
        }
    }

    /// Moves every row from slot `order[i]` to slot `i`; `order` is a
    /// permutation of all slots. Live rows must come first, so the free slots
    /// end up at the back. Vectors move in place, one cycle at a time, so the
    /// biggest column is never copied whole.
    fn permute(&mut self, order: &[u32]) {
        fn gather<T: Copy>(v: &[T], order: &[u32]) -> Vec<T> {
            order.iter().map(|&s| v[s as usize]).collect()
        }
        self.pks = gather(&self.pks, order);
        self.live = gather(&self.live, order);
        let live = self.live.iter().filter(|&&l| l).count();
        debug_assert!(self.live[..live].iter().all(|&l| l));
        self.free = (live as u32..order.len() as u32).rev().collect();
        for c in &mut self.cols {
            match c {
                ColData::Pk => {}
                ColData::Int(v) => *v = gather(v, order),
                ColData::Str(s) => {
                    s.spans = gather(&s.spans, order);
                    if let Some(d) = &mut s.dict {
                        d.codes = gather(&d.codes, order);
                    }
                }
                ColData::Vector(c) => c.permute(order),
            }
        }
    }

    fn vectors(&self, col: usize) -> Option<&VecCol> {
        match self.cols.get(col)? {
            ColData::Vector(c) => Some(c),
            _ => None,
        }
    }

    fn view(&self, slot: usize) -> RowView<'_> {
        RowView { pk: self.pks[slot], src: Src::Cache(self, slot) }
    }

    fn bytes(&self) -> usize {
        self.pks.capacity() * 8
            + self.live.capacity()
            + self.free.capacity() * 4
            + self.cols.iter().map(ColData::bytes).sum::<usize>()
    }
}

/// A column predicate bound to the cached column it reads.
enum Probe<'a> {
    Int(&'a [i64], CmpOp, i64),
    Str(&'a StrCol, CmpOp, &'a [u8]),
    /// `=` (true) or `<>` (false) on a dictionary-coded string column.
    Code(&'a [u32], bool, u32),
}

impl<'a> Probe<'a> {
    fn new(cache: &'a RowCache, p: &ColPred<'a>) -> Option<Probe<'a>> {
        Some(match (cache.cols.get(p.col)?, &p.value) {
            (ColData::Pk, PredValue::Int(v)) => Probe::Int(&cache.pks, p.op, *v),
            (ColData::Int(c), PredValue::Int(v)) => Probe::Int(c, p.op, *v),
            (ColData::Str(c), PredValue::Str(v)) => match (p.op, c.code(v), &c.dict) {
                (CmpOp::Eq | CmpOp::Ne, Some(code), Some(d)) => Probe::Code(&d.codes, p.op == CmpOp::Eq, code),
                _ => Probe::Str(c, p.op, v.as_bytes()),
            },
            _ => return None,
        })
    }

    /// Live slots where the predicate holds, in slot order.
    fn select(&self, live: &[bool]) -> Vec<u32> {
        match self {
            // Free slots have NO_CODE, which equals no constant's code.
            Probe::Code(c, true, v) => (0..c.len()).filter(|&s| c[s] == *v).map(|s| s as u32).collect(),
            _ => (0..live.len()).filter(|&s| live[s] && self.holds(s)).map(|s| s as u32).collect(),
        }
    }

    #[inline]
    fn holds(&self, slot: usize) -> bool {
        match self {
            Probe::Int(c, op, v) => holds(*op, c[slot].cmp(v)),
            Probe::Code(c, eq, v) => (c[slot] == *v) == *eq,
            Probe::Str(c, op, v) => {
                let (off, len) = c.spans[slot];
                holds(*op, c.arena.as_bytes()[off..off + len].cmp(v))
            }
        }
    }
}

/// Live slots where every probe holds (at least one probe).
fn select(cache: &RowCache, probes: &[Probe<'_>]) -> Vec<u32> {
    let mut sel = probes[0].select(&cache.live);
    for p in &probes[1..] {
        sel.retain(|&s| p.holds(s as usize));
    }
    sel
}

pub struct Table {
    pub def: TableDef,
    pub rows: BTreeMap<i64, Entry>,
    /// When present, holds every row of `rows`.
    cache: Option<RowCache>,
    /// The cache was set aside by `defer_cache`; `load_cache` rebuilds it.
    deferred: bool,
    /// A statement has read the table since the database was opened.
    pub read: bool,
    /// The table's vector column, if it has one.
    vector_col: Option<usize>,
    /// Index over the cached vector column, built on first need (see
    /// `prepare_index`) and then kept in step with every committed change:
    /// k-means lists, and 8-bit codes of every vector.
    index: Option<Ivf>,
    codes: Option<Sq8>,
    /// Rows when the index was last built (0: not built).
    indexed_rows: usize,
    /// How much of the table filtered TopK scans through the lists visit.
    filtered_lists: Cell<ListUse>,
    /// The same for unfiltered scans, kept only while they can be split.
    unfiltered_lists: Cell<ListUse>,
    /// The helper thread, if any, and the table size from which unfiltered
    /// scans use it, looked up once instead of at every query.
    helper: Option<&'static Helper>,
    split_rows: usize,
}

/// Average fraction of rows a filtered TopK through the lists visited, and
/// the number of filtered TopK queries so far. When the lists stop pruning
/// (wide clusters, uniform data), selecting the matching rows directly is
/// cheaper, so filtered queries skip the lists except every 16th one, which
/// keeps the average current. Unfiltered queries that can be split keep an
/// average of their own and skip the lists the same way, for a scan of both
/// halves of the slots at once.
#[derive(Clone, Copy, Default)]
struct ListUse {
    visited: f32,
    queries: u32,
}

/// Slots handed to the codes at a time.
const BLOCK: usize = 64;

/// Candidate slots for `Table::topk`: neighbouring slots, a selection
/// spread over the table, or the live slots of a range.
enum Cand<'a> {
    Run(&'a [u32]),
    Scattered(&'a [u32]),
    Live(Range<usize>),
}

/// Offers to a TopK the candidates (all passing the predicates) that the
/// codes keep, tightening the codes' bound whenever the K-th distance drops.
struct Offer<'a> {
    cache: &'a RowCache,
    col: usize,
    codes: Option<Sq8Query<'a>>,
    kth: Option<f64>,
    kept: Vec<u32>,
    /// A candidate's floats, when the column holds codes.
    floats: Vec<f32>,
}

impl<'a> Offer<'a> {
    fn new(cache: &'a RowCache, col: usize, codes: Option<&'a Sq8>, top: &TopK<'_>) -> Offer<'a> {
        let mut codes = codes.map(|c| c.query(top.query()));
        let kth = top.kth();
        if let (Some(c), Some(k)) = (&mut codes, kth) {
            c.set_kth(k);
        }
        Offer { cache, col, codes, kth, kept: Vec::with_capacity(BLOCK), floats: Vec::new() }
    }

    #[inline(always)]
    fn run(&mut self, cand: Cand<'_>, top: &mut TopK<'_>, skip: &impl Fn(i64) -> bool, rest: &impl Fn(&RowView<'_>) -> bool) {
        let cache = self.cache;
        self.kept.clear();
        match (&self.codes, cand) {
            (Some(c), Cand::Run(slots)) => c.keep(slots, false, &mut self.kept),
            (Some(c), Cand::Scattered(slots)) => c.keep(slots, true, &mut self.kept),
            (Some(c), Cand::Live(range)) => c.keep_live(range, &cache.live, &mut self.kept),
            (None, Cand::Run(slots) | Cand::Scattered(slots)) => self.kept.extend_from_slice(slots),
            (None, Cand::Live(range)) => self.kept.extend(range.filter(|&s| cache.live[s]).map(|s| s as u32)),
        }
        for &s in &self.kept {
            let row = cache.view(s as usize);
            if skip(row.pk) || !rest(&row) {
                continue;
            }
            if let ValRef::Vector(v) = cache.get(s as usize, self.col) {
                top.offer(row.pk, v.floats(&mut self.floats));
                let k = top.kth();
                if k != self.kth {
                    self.kth = k;
                    if let (Some(c), Some(k)) = (&mut self.codes, k) {
                        c.set_kth(k);
                    }
                }
            }
        }
    }
}

/// Offers the live slots of `slots` (starting on a multiple of BLOCK) to
/// `top`, a block at a time.
fn scan_live(
    cache: &RowCache,
    col: usize,
    codes: Option<&Sq8>,
    slots: Range<usize>,
    skip: &impl Fn(i64) -> bool,
    rest: &impl Fn(&RowView<'_>) -> bool,
    top: &mut TopK<'_>,
) {
    let mut offer = Offer::new(cache, col, codes, top);
    for start in slots.clone().step_by(BLOCK) {
        offer.run(Cand::Live(start..(start + BLOCK).min(slots.end)), top, skip, rest);
    }
}

impl Table {
    pub fn new(def: TableDef) -> Table {
        let cache = Some(RowCache::new(&def));
        let vector_col = def.columns.iter().position(|c| matches!(c.ty, DataType::Vector(_)));
        Table { def, rows: BTreeMap::new(), cache, deferred: false, read: false, vector_col, index: None, codes: None, indexed_rows: 0, filtered_lists: Cell::default(), unfiltered_lists: Cell::default(), helper: helper::get(), split_rows: helper::min_rows() }
    }

    pub fn is_cached(&self) -> bool {
        self.cache.is_some()
    }

    /// Whether the table has k-means lists and 8-bit codes right now.
    #[cfg(test)]
    pub fn index_parts(&self) -> (bool, bool) {
        (self.index.is_some(), self.codes.is_some())
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache.as_ref().map_or(0, RowCache::bytes)
            + self.index.as_ref().map_or(0, Ivf::bytes)
            + self.codes.as_ref().map_or(0, Sq8::bytes)
    }

    /// Switches the table to reading rows from the log.
    pub fn drop_cache(&mut self) {
        self.cache = None;
        self.index = None;
        self.codes = None;
        self.indexed_rows = 0;
        for e in self.rows.values_mut() {
            e.slot = NO_SLOT;
        }
    }

    /// Sets the cache aside until a statement reads the table, so a table
    /// that is only written to (a bulk load) keeps no decoded rows in memory.
    pub fn defer_cache(&mut self) {
        if self.cache.is_some() {
            self.drop_cache();
            self.deferred = true;
        }
    }

    /// Marks the table as read and rebuilds a deferred cache from the log.
    /// A table that would take more than `budget` bytes stays read from the
    /// log, as if its cache had been dropped for size.
    ///
    /// The log is read front to back, the way recovery reads it, which
    /// drops its pages from the page cache behind itself: reading row by
    /// row instead left the whole log in the page cache next to the new
    /// cache. Of all the versions of a row in the log, the live one is the
    /// one its index entry points at.
    pub fn load_cache(&mut self, log: &LogStore, budget: usize) -> DbResult<()> {
        self.read = true;
        if !std::mem::take(&mut self.deferred) {
            return Ok(());
        }
        let too_big = |name: &str| {
            eprintln!("engine: table {name} does not fit the {budget} byte row cache; reading it from disk");
        };
        if RowCache::estimate(&self.def, self.rows.len()) > budget {
            too_big(&self.def.name);
            return Ok(());
        }
        let mut cache = RowCache::new(&self.def);
        cache.reserve(self.rows.len());
        let (def, rows) = (&self.def, &mut self.rows);
        let (mut loaded, mut fits) = (0usize, true);
        let scanned = log.scan(|_, ops| {
            for op in ops {
                let OpView::Put { table, pk, row, loc } = op else { continue };
                if !fits || *table != def.id {
                    continue;
                }
                let Some(e) = rows.get_mut(pk).filter(|e| e.loc == *loc) else { continue };
                e.slot = cache.insert(*pk, &decode_row(def, *pk, row)?);
                loaded += 1;
                fits = cache.bytes() <= budget;
            }
            Ok(())
        });
        if scanned.is_ok() && fits && loaded == self.rows.len() {
            self.cache = Some(cache);
            return Ok(());
        }
        // No cache after all: no entry may keep a slot of the abandoned one.
        for e in self.rows.values_mut() {
            e.slot = NO_SLOT;
        }
        scanned?;
        if fits {
            bail!(Internal, "the log holds {} of the {} rows of table {}", loaded, self.rows.len(), self.def.name);
        }
        too_big(&self.def.name);
        Ok(())
    }

    /// Records a committed insert or update. `row` is required while the
    /// table is cached; without it the cache is given up.
    pub fn apply_put(&mut self, pk: i64, loc: RowLoc, row: Option<Vec<Value>>) {
        if row.is_none() && self.cache.is_some() {
            self.drop_cache();
        }
        let old = self.rows.get(&pk).map_or(NO_SLOT, |e| e.slot);
        let slot = match (&mut self.cache, row) {
            (Some(cache), Some(row)) => {
                let index = &mut self.index;
                if old != NO_SLOT {
                    cache.remove(old);
                    if let Some(ix) = index {
                        ix.remove(old);
                    }
                }
                let slot = cache.insert(pk, &row);
                if let Some(Value::Vector(v)) = self.vector_col.map(|col| &row[col]) {
                    if let Some(ix) = index {
                        ix.insert(slot, v);
                    }
                    if let Some(codes) = &mut self.codes {
                        codes.set(slot, v);
                    }
                }
                slot
            }
            _ => NO_SLOT,
        };
        self.rows.insert(pk, Entry { loc, slot });
    }

    /// Records a committed update of some columns of an existing row: its
    /// new location and, if the row is cached, the new values in place.
    pub fn apply_update(&mut self, pk: i64, loc: RowLoc, sets: &[(usize, Value)]) {
        let Some(e) = self.rows.get_mut(&pk) else {
            return;
        };
        e.loc = loc;
        let slot = e.slot;
        let (Some(cache), true) = (&mut self.cache, slot != NO_SLOT) else {
            return;
        };
        for (col, value) in sets {
            cache.cols[*col].set(slot as usize, value);
            if let (true, Value::Vector(v)) = (Some(*col) == self.vector_col, value) {
                if let Some(ix) = &mut self.index {
                    ix.remove(slot);
                    ix.insert(slot, v);
                }
                if let Some(codes) = &mut self.codes {
                    codes.set(slot, v);
                }
            }
        }
    }

    pub fn apply_delete(&mut self, pk: i64) {
        if let Some(e) = self.rows.remove(&pk) {
            if let (Some(cache), true) = (&mut self.cache, e.slot != NO_SLOT) {
                cache.remove(e.slot);
                if let Some(ix) = &mut self.index {
                    ix.remove(e.slot);
                }
            }
        }
    }

    /// Committed row by primary key.
    pub fn get(&self, log: &LogStore, pk: i64) -> DbResult<Option<Vec<Value>>> {
        let Some(e) = self.rows.get(&pk) else {
            return Ok(None);
        };
        if let (Some(cache), true) = (&self.cache, e.slot != NO_SLOT) {
            return Ok(Some(cache.view(e.slot as usize).to_values()));
        }
        let mut buf = Vec::new();
        log.read(e.loc, &mut buf)?;
        Ok(Some(decode_row(&self.def, pk, &buf)?))
    }

    /// Visits committed rows with pk in `range` in pk order until `f`
    /// returns false.
    pub fn scan(
        &self,
        log: &LogStore,
        range: RangeInclusive<i64>,
        mut f: impl FnMut(RowView<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let mut buf = Vec::new();
        for (&pk, e) in self.rows.range(range) {
            let go = match (&self.cache, e.slot != NO_SLOT) {
                (Some(cache), true) => f(cache.view(e.slot as usize))?,
                _ => {
                    log.read(e.loc, &mut buf)?;
                    let values = decode_row(&self.def, pk, &buf)?;
                    f(RowView::new(pk, &values))?
                }
            };
            if !go {
                break;
            }
        }
        Ok(())
    }

    /// Builds the vector index for `topk` if the table has none or has more
    /// than doubled since it was built, as long as the table is cached, has a
    /// vector column and at least `opts.min_rows` rows. With k-means lists
    /// (tables of at most `opts.max_list_rows` rows) the cached rows are then
    /// laid out list by list, so a TopK scan through the lists reads memory
    /// in order even when it has to visit every list. Lists built while the
    /// table was smaller are kept up to date until the next rebuild, which
    /// drops them once the table is past the cap.
    pub fn prepare_index(&mut self, opts: IndexOptions) {
        let (Some(cache), Some(col)) = (&mut self.cache, self.vector_col) else {
            return;
        };
        let n = self.rows.len();
        if n == 0 || n < opts.min_rows || (self.indexed_rows > 0 && n <= 2 * self.indexed_rows) {
            return;
        }
        self.indexed_rows = n;
        self.index = None;
        if opts.lists && n <= opts.max_list_rows {
            let Some(vectors) = cache.vectors(col) else {
                return;
            };
            let mut ix = Ivf::build(vectors, &cache.live);
            let order = ix.slot_order(cache.live.len());
            cache.permute(&order);
            let mut new_slot = vec![NO_SLOT; order.len()];
            for (new, &old) in order.iter().enumerate() {
                new_slot[old as usize] = new as u32;
            }
            for e in self.rows.values_mut() {
                e.slot = new_slot[e.slot as usize];
            }
            ix.remap(&new_slot);
            self.index = Some(ix);
        }
        if opts.codes {
            if let Some(vectors) = cache.vectors(col) {
                self.codes = Some(Sq8::build(vectors, &cache.live));
            }
        }
    }


    /// Offers to `top` every committed row of a cached table where the
    /// predicates and `rest` hold and `skip` does not. With k-means lists,
    /// lists are visited by increasing lower bound and the scan stops at the
    /// first one that cannot beat the K-th distance so far; with codes, rows
    /// they prove too far are passed over without reading their floats.
    /// Without predicates, a table of `helper::min_rows()` rows or more is
    /// scanned slot by slot in two halves, one on the helper thread, each
    /// into a TopK of its own. Either way `top` ends up as if every row had
    /// been offered.
    pub fn topk(
        &self,
        preds: &[ColPred<'_>],
        skip: impl Fn(i64) -> bool + Sync,
        rest: impl Fn(&RowView<'_>) -> bool + Sync,
        top: &mut TopK<'_>,
    ) -> DbResult<()> {
        let (Some(cache), Some(col)) = (&self.cache, self.vector_col) else {
            bail!(Internal, "cached TopK on a table without cached vectors");
        };
        let probes: Option<Vec<Probe<'_>>> = preds.iter().map(|p| Probe::new(cache, p)).collect();
        let passes = |s: usize| match &probes {
            Some(probes) => probes.iter().all(|p| p.holds(s)),
            None => preds.iter().all(|p| p.test(&cache.view(s))),
        };
        let mut offer = Offer::new(cache, col, self.codes.as_ref(), top);
        let mut run = |cand: Cand<'_>, top: &mut TopK<'_>| offer.run(cand, top, &skip, &rest);
        let filtered = !preds.is_empty();
        // Unfiltered scans of a large table are split between two threads,
        // over the slots rather than the lists, so they take the lists only
        // while the lists prune.
        let helper = if filtered || self.rows.len() < self.split_rows { None } else { self.helper };
        let uses = if filtered { Some(&self.filtered_lists) } else { helper.map(|_| &self.unfiltered_lists) };
        let lists = self.index.as_ref().filter(|_| {
            let Some(uses) = uses else {
                return true;
            };
            let mut u = uses.get();
            u.queries = u.queries.wrapping_add(1);
            uses.set(u);
            u.visited <= 0.5 || u.queries.is_multiple_of(16)
        });
        let mut block: Vec<u32> = Vec::with_capacity(BLOCK);
        if let Some(ix) = lists {
            let mut visited = 0usize;
            for (bound, l) in ix.order(top.query()) {
                if top.kth().is_some_and(|kth| bound > kth) {
                    break;
                }
                let list = ix.list(l);
                visited += list.len();
                for chunk in list.chunks(BLOCK) {
                    if filtered {
                        block.clear();
                        block.extend(chunk.iter().filter(|&&s| passes(s as usize)));
                        run(Cand::Scattered(&block), top);
                    } else {
                        run(Cand::Run(chunk), top);
                    }
                }
            }
            if let Some(uses) = uses {
                let mut u = uses.get();
                u.visited = 0.75 * u.visited + 0.25 * (visited as f32 / self.rows.len().max(1) as f32);
                uses.set(u);
            }
            return Ok(());
        }
        match (&probes, helper) {
            (Some(probes), _) if !probes.is_empty() => {
                for chunk in select(cache, probes).chunks(BLOCK) {
                    run(Cand::Scattered(chunk), top);
                }
            }
            (_, Some(h)) => {
                // Lower half here, upper half on the helper thread into a
                // TopK of its own, merged at the end.
                let n = cache.live.len();
                let mid = n / 2 / BLOCK * BLOCK;
                let codes = self.codes.as_ref();
                let mut theirs = TopK::new(top.query(), top.k());
                let (skip, rest) = (&skip, &rest);
                h.join(
                    || scan_live(cache, col, codes, 0..mid, skip, rest, top),
                    || scan_live(cache, col, codes, mid..n, skip, rest, &mut theirs),
                );
                top.merge(theirs);
            }
            _ => {
                for start in (0..cache.live.len()).step_by(BLOCK) {
                    let end = (start + BLOCK).min(cache.live.len());
                    if filtered {
                        // Predicates the cache cannot probe: tested row by row.
                        block.clear();
                        block.extend((start..end).filter(|&s| cache.live[s] && passes(s)).map(|s| s as u32));
                        run(Cand::Scattered(&block), top);
                    } else {
                        run(Cand::Live(start..end), top);
                    }
                }
            }
        }
        Ok(())
    }

    /// Like `scan` restricted to rows where every predicate holds, in no
    /// particular order. A cached table scanned over the whole key range is
    /// walked slot by slot without the index, and the predicates run as
    /// loops over their column arrays before any row is looked at.
    pub fn scan_unordered(
        &self,
        log: &LogStore,
        range: RangeInclusive<i64>,
        preds: &[ColPred<'_>],
        mut f: impl FnMut(RowView<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let whole = *range.start() == i64::MIN && *range.end() == i64::MAX;
        let probes = match &self.cache {
            Some(cache) if whole => preds.iter().map(|p| Probe::new(cache, p)).collect::<Option<Vec<_>>>(),
            _ => None,
        };
        let (Some(cache), Some(probes)) = (&self.cache, probes) else {
            return self.scan(log, range, |row| if preds.iter().all(|p| p.test(&row)) { f(row) } else { Ok(true) });
        };
        if probes.is_empty() {
            for (slot, &live) in cache.live.iter().enumerate() {
                if live && !f(cache.view(slot))? {
                    break;
                }
            }
            return Ok(());
        }
        for s in select(cache, &probes) {
            if !f(cache.view(s as usize))? {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::catalog::Column;

    fn def() -> TableDef {
        TableDef {
            id: 1,
            name: "t".into(),
            columns: vec![
                Column { name: "s".into(), ty: DataType::Varchar(8) },
                Column { name: "id".into(), ty: DataType::BigInt },
                Column { name: "v".into(), ty: DataType::Vector(2) },
                Column { name: "n".into(), ty: DataType::BigInt },
            ],
            pk: 1,
            vector_index: None,
        }
    }

    fn row(pk: i64, s: &str, n: i64) -> Vec<Value> {
        vec![Value::Str(s.into()), Value::Int(pk), Value::Vector(vec![pk as f32, -1.0]), Value::Int(n)]
    }

    /// The cached TopK equals offering every row of a model, with every
    /// combination of lists and codes (and lists left out above the row
    /// cap), before and after the rows are relaid out, through updates and
    /// deletes, with and without filters (`=`, `<>`, a value no row has, two
    /// predicates).
    #[test]
    fn cached_topk_matches_model() {
        for (lists, codes) in [(true, true), (true, false), (false, true), (false, false)] {
            check_cached_topk(IndexOptions { min_rows: 1, max_list_rows: usize::MAX, lists, codes });
        }
        check_cached_topk(IndexOptions { min_rows: 1, max_list_rows: 2000, lists: true, codes: true });
    }

    /// An unfiltered TopK over a table large enough to be split between two
    /// threads returns what offering every row does, through the lists (told
    /// they prune nothing, or left to measure it) or without them, with codes
    /// or without (at 96 dimensions their scan stops early for some rows),
    /// while skipping some rows and testing others.
    #[test]
    fn split_topk_matches_model() {
        let configs = [(true, true, true), (true, true, false), (true, false, true), (false, true, false), (false, false, false)];
        for (dim, (lists, codes, unpruned)) in [16usize, 96].into_iter().flat_map(|dim| configs.map(|c| (dim, c))) {
            let def = TableDef {
                id: 1,
                name: "t".into(),
                columns: vec![Column { name: "id".into(), ty: DataType::BigInt }, Column { name: "v".into(), ty: DataType::Vector(dim as u32) }],
                pk: 0,
                vector_index: None,
            };
            let mut x = 7u64;
            let mut point = move || -> Vec<f32> {
                (0..dim)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        // Quarter steps make exact distance ties common.
                        (((x >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * 8.0).round() / 4.0
                    })
                    .collect()
            };
            let loc = RowLoc { seg: 0, off: 0, len: 0 };
            let mut table = Table::new(def);
            let mut model: BTreeMap<i64, Vec<f32>> = BTreeMap::new();
            let mut put = |table: &mut Table, pk: i64, v: Vec<f32>| {
                table.apply_put(pk, loc, Some(vec![Value::Int(pk), Value::Vector(v.clone())]));
                model.insert(pk, v);
            };
            for pk in 0..6000 {
                put(&mut table, pk, point());
            }
            table.prepare_index(IndexOptions { min_rows: 1, max_list_rows: usize::MAX, lists, codes });
            for pk in 6000..6400 {
                put(&mut table, pk, point());
            }
            for pk in (0..6400).step_by(5) {
                table.apply_delete(pk);
                model.remove(&pk);
            }
            assert!(table.rows.len() >= table.split_rows);
            if unpruned {
                table.unfiltered_lists.set(ListUse { visited: 1.0, queries: 0 });
            }
            for qi in 0..60 {
                let q = point();
                let k = [1, 10, 50][qi % 3];
                let skip = |pk: i64| pk % 13 == 0;
                let rest = |row: &RowView<'_>| row.pk % 7 != 3;
                let mut want = TopK::new(&q, k);
                for (&pk, v) in model.iter().filter(|(&pk, _)| !skip(pk) && pk % 7 != 3) {
                    want.offer(pk, v);
                }
                let mut got = TopK::new(&q, k);
                table.topk(&[], skip, rest, &mut got).unwrap();
                assert_eq!(got.into_sorted(), want.into_sorted(), "dim {dim} lists {lists} codes {codes} unpruned {unpruned} query {qi}");
            }
        }
    }

    /// Lists built below the row cap are dropped by the first rebuild above
    /// it; the codes stay.
    #[test]
    fn lists_are_capped() {
        let opts = IndexOptions { min_rows: 1, max_list_rows: 100, lists: true, codes: true };
        let mut table = Table::new(def());
        let loc = RowLoc { seg: 0, off: 0, len: 0 };
        for pk in 0..80 {
            table.apply_put(pk, loc, Some(row(pk, "a", pk)));
        }
        table.prepare_index(opts);
        assert!(table.index.is_some() && table.codes.is_some());
        for pk in 80..200 {
            table.apply_put(pk, loc, Some(row(pk, "a", pk)));
        }
        table.prepare_index(opts);
        assert!(table.index.is_none() && table.codes.is_some());
        let q = [150.2, -1.0];
        let mut got = TopK::new(&q, 3);
        table.topk(&[], |_| false, |_| true, &mut got).unwrap();
        assert_eq!(got.into_sorted(), vec![150, 151, 149]);
    }

    fn check_cached_topk(opts: IndexOptions) {
        let def = TableDef {
            id: 1,
            name: "t".into(),
            columns: vec![
                Column { name: "id".into(), ty: DataType::BigInt },
                Column { name: "cat".into(), ty: DataType::Varchar(4) },
                Column { name: "v".into(), ty: DataType::Vector(8) },
            ],
            pk: 0,
            vector_index: None,
        };
        let mut x = 99u64;
        let mut unit = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        };
        let centers: Vec<Vec<f32>> = (0..12).map(|_| (0..8).map(|_| unit() * 5.0).collect()).collect();
        let mut point = |c: usize| -> Vec<f32> {
            // Quarter steps make exact distance ties common.
            centers[c % 12].iter().map(|v| ((v + unit() * 0.6) * 4.0).round() / 4.0).collect()
        };
        let loc = RowLoc { seg: 0, off: 0, len: 0 };
        let mut table = Table::new(def);
        let mut model: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
        let put = |table: &mut Table, model: &mut BTreeMap<i64, Vec<Value>>, pk: i64, v: Vec<f32>| {
            let row = vec![Value::Int(pk), Value::Str(format!("c{}", pk % 3)), Value::Vector(v)];
            table.apply_put(pk, loc, Some(row.clone()));
            model.insert(pk, row);
        };
        for pk in 0..3000 {
            put(&mut table, &mut model, pk, point(pk as usize));
        }
        for pk in (0..3000).step_by(5) {
            table.apply_delete(pk);
            model.remove(&pk);
        }
        table.prepare_index(opts);
        assert_eq!(table.index.is_some(), opts.lists && table.rows.len() <= opts.max_list_rows);
        assert_eq!(table.codes.is_some(), opts.codes);
        for pk in (1..3000).step_by(7) {
            // Some far outside the range the codes were fitted to.
            let v = point(pk as usize + 5).iter().map(|c| if pk % 3 == 0 { c * 9.0 } else { *c }).collect();
            put(&mut table, &mut model, pk, v);
        }
        for pk in 3000..3400 {
            put(&mut table, &mut model, pk, point(pk as usize));
        }
        for pk in (2..3400).step_by(11) {
            table.apply_delete(pk);
            model.remove(&pk);
        }
        for (pk, row) in &model {
            let e = table.rows[pk];
            assert_eq!(&table.cache.as_ref().unwrap().view(e.slot as usize).to_values(), row);
        }
        for qi in 0..200 {
            let q = point(qi);
            let k = [1, 5, 10, 50][qi % 4];
            let cat = format!("c{}", qi % 3);
            let preds: Vec<ColPred<'_>> = match qi % 5 {
                1 => vec![ColPred { col: 1, op: CmpOp::Eq, value: PredValue::Str(&cat) }],
                2 => vec![ColPred { col: 1, op: CmpOp::Ne, value: PredValue::Str(&cat) }],
                3 => vec![ColPred { col: 1, op: CmpOp::Eq, value: PredValue::Str("zz") }],
                4 => vec![
                    ColPred { col: 1, op: CmpOp::Eq, value: PredValue::Str(&cat) },
                    ColPred { col: 0, op: CmpOp::Gt, value: PredValue::Int(1500) },
                ],
                _ => Vec::new(),
            };
            let mut want = TopK::new(&q, k);
            for (&pk, row) in &model {
                let view = RowView::new(pk, row);
                if preds.iter().all(|p| p.test(&view)) {
                    if let Value::Vector(v) = &row[2] {
                        want.offer(pk, v);
                    }
                }
            }
            let mut got = TopK::new(&q, k);
            table.topk(&preds, |_| false, |_| true, &mut got).unwrap();
            assert_eq!(got.into_sorted(), want.into_sorted(), "{opts:?} query {qi}");
        }
    }

    /// Updating some columns of a row in place leaves the table as writing
    /// the whole new row does: the same rows, and the same TopK with every
    /// combination of lists and codes.
    #[test]
    fn apply_update_equals_apply_put() {
        let def = TableDef {
            id: 1,
            name: "t".into(),
            columns: vec![
                Column { name: "id".into(), ty: DataType::BigInt },
                Column { name: "cat".into(), ty: DataType::Varchar(8) },
                Column { name: "n".into(), ty: DataType::BigInt },
                Column { name: "v".into(), ty: DataType::Vector(8) },
            ],
            pk: 0,
            vector_index: None,
        };
        for (lists, codes) in [(true, true), (true, false), (false, true), (false, false)] {
            let opts = IndexOptions { min_rows: 1, max_list_rows: usize::MAX, lists, codes };
            let mut x = 7u64;
            let mut next = move || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x
            };
            let mut vector = |scale: f32| -> Vec<f32> {
                (0..8).map(|_| ((next() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * scale).collect()
            };
            let loc = |n: u64| RowLoc { seg: 1, off: n, len: 9 };
            let (mut whole, mut in_place) = (Table::new(def.clone()), Table::new(def.clone()));
            let mut model: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
            for pk in 0..600 {
                let row = vec![Value::Int(pk), Value::Str(format!("c{}", pk % 3)), Value::Int(pk * 2), Value::Vector(vector(1.0))];
                whole.apply_put(pk, loc(0), Some(row.clone()));
                in_place.apply_put(pk, loc(0), Some(row.clone()));
                model.insert(pk, row);
            }
            whole.prepare_index(opts);
            in_place.prepare_index(opts);
            for step in 0..900u64 {
                let pk = (step * 7919 % 600) as i64;
                // One, two or three columns; some vectors far outside the
                // range the codes were fitted to.
                let mut sets = Vec::new();
                if step % 2 == 0 {
                    sets.push((1, Value::Str(format!("s{}", step % 5))));
                }
                if step % 3 == 0 {
                    sets.push((2, Value::Int(step as i64)));
                }
                if step % 4 != 1 {
                    sets.push((3, Value::Vector(vector(if step % 9 == 0 { 40.0 } else { 1.0 }))));
                }
                let row = model.get_mut(&pk).unwrap();
                for (col, v) in &sets {
                    row[*col] = v.clone();
                }
                whole.apply_put(pk, loc(step + 1), Some(row.clone()));
                in_place.apply_update(pk, loc(step + 1), &sets);
            }
            for (pk, row) in &model {
                let e = in_place.rows[pk];
                assert_eq!(e.loc, whole.rows[pk].loc);
                assert_eq!(&in_place.cache.as_ref().unwrap().view(e.slot as usize).to_values(), row, "{opts:?} row {pk}");
            }
            for qi in 0..100u64 {
                let q = vector(if qi % 10 == 0 { 30.0 } else { 1.0 });
                let cat = format!("s{}", qi % 5);
                let preds: Vec<ColPred<'_>> =
                    if qi % 2 == 0 { Vec::new() } else { vec![ColPred { col: 1, op: CmpOp::Eq, value: PredValue::Str(&cat) }] };
                let (mut a, mut b) = (TopK::new(&q, 10), TopK::new(&q, 10));
                whole.topk(&preds, |_| false, |_| true, &mut a).unwrap();
                in_place.topk(&preds, |_| false, |_| true, &mut b).unwrap();
                let mut want = TopK::new(&q, 10);
                for (&pk, row) in &model {
                    if preds.iter().all(|p| p.test(&RowView::new(pk, row))) {
                        if let Value::Vector(v) = &row[3] {
                            want.offer(pk, v);
                        }
                    }
                }
                let want = want.into_sorted();
                assert_eq!(b.into_sorted(), want, "{opts:?} query {qi}, in place");
                assert_eq!(a.into_sorted(), want, "{opts:?} query {qi}, whole rows");
            }
        }
    }

    /// Random puts and deletes against a model map, with string sizes that
    /// force arena compaction and slot reuse.
    #[test]
    fn cache_matches_model_through_updates_deletes_and_compaction() {
        let def = def();
        let mut cache = RowCache::new(&def);
        let mut slots: BTreeMap<i64, u32> = BTreeMap::new();
        let mut model: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
        let mut x = 12345u64;
        for step in 0..20_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let pk = ((x >> 33) % 300) as i64 - 150;
            if (x >> 20).is_multiple_of(4) {
                if let Some(slot) = slots.remove(&pk) {
                    cache.remove(slot);
                }
                model.remove(&pk);
            } else {
                let s = "é".repeat(((x >> 40) % 6) as usize) + &step.to_string();
                if let Some(slot) = slots.remove(&pk) {
                    cache.remove(slot);
                }
                slots.insert(pk, cache.insert(pk, &row(pk, &s, step)));
                model.insert(pk, row(pk, &s, step));
            }
        }
        for (pk, slot) in &slots {
            let view = cache.view(*slot as usize);
            assert_eq!(view.pk, *pk);
            assert_eq!(view.to_values(), model[pk]);
        }
        assert_eq!(cache.live.iter().filter(|l| **l).count(), model.len());
        let ColData::Str(s) = &cache.cols[0] else { panic!() };
        let live_bytes: usize = model.values().map(|r| if let Value::Str(s) = &r[0] { s.len() } else { 0 }).sum();
        assert!(s.arena.len() <= 2 * live_bytes + 4096 + 64, "arena {} for {} live bytes", s.arena.len(), live_bytes);
    }
}
