#!/usr/bin/env python3
"""Differential tester for minivectordb.

Random SQL sessions run against the database (JSON-lines output) and against
a small reference model in this file; any difference is reported with the
seed and the statements so far. With --crash the database is also killed at
random points, sometimes while a statement is in flight, and after restart
its contents must equal the committed state of the model (for an in-flight
statement: the state before or after it, never a mix).

    tests/difftest.py --bin target/release/minivectordb --seeds 0:100 --steps 300 [--crash]

Statements are generated in a structured form; the model executes that form
and the database gets the same statement rendered as SQL. Vector components
are multiples of 1/4, so every distance is computed exactly by both sides.
"""

import argparse
import json
import math
import os
import random
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import time

I64_MIN, I64_MAX = -(2**63), 2**63 - 1
STATS = __import__("collections").Counter()


class ModelError(Exception):
    def __init__(self, kind):
        super().__init__(kind)
        self.kind = kind


# ---------------------------------------------------------------- literals


class Lit:
    """SQL literal: kind is 'int', 'num' (decimal text) or 'str'."""

    def __init__(self, kind, value):
        self.kind, self.value = kind, value

    def sql(self):
        if self.kind == "int":
            return str(self.value)
        if self.kind == "num":
            return self.value
        return "'" + self.value.replace("\\", "\\\\").replace("'", "''") + "'"


NUM_RE = re.compile(r"^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$")


def f32(x):
    return struct.unpack("<f", struct.pack("<f", x))[0]


def parse_vector(text):
    t = text.strip()
    if not (t.startswith("[") and t.endswith("]")):
        raise ModelError("InvalidValue")
    inner = t[1:-1]
    if not inner.strip():
        return ()
    out = []
    for e in inner.split(","):
        e = e.strip()
        if not NUM_RE.match(e):
            raise ModelError("InvalidValue")
        try:
            out.append(f32(float(e)))
        except OverflowError:
            raise ModelError("InvalidValue")
    return tuple(out)


def parse_number(text):
    v = float(text)
    if math.isinf(v):
        raise ModelError("OutOfRange")
    return v


def l2(a, b):
    return math.sqrt(sum((x - y) ** 2 for x, y in zip(a, b)))


# ---------------------------------------------------------------- schema


class Col:
    def __init__(self, name, kind, param):
        self.name, self.kind, self.param = name, kind, param  # kind: int | str | vec

    def sql_type(self):
        return {"int": "BIGINT", "str": f"VARCHAR({self.param})", "vec": f"VECTOR({self.param})"}[self.kind]


class Table:
    def __init__(self, name, cols, pk):
        self.name, self.cols, self.pk = name, cols, pk
        self.vec = next((i for i, c in enumerate(cols) if c.kind == "vec"), None)
        self.index = None
        self.rows = {}

    def clone(self):
        t = Table(self.name, self.cols, self.pk)
        t.index = self.index
        t.rows = dict(self.rows)
        return t

    def col(self, name):
        for i, c in enumerate(self.cols):
            if c.name.lower() == name.lower():
                return i
        raise ModelError("NoSuchColumn")


def clone_db(db):
    return {k: t.clone() for k, t in db.items()}


def coerce(lit, col):
    if col.kind == "int":
        if lit.kind == "int":
            return lit.value
        if lit.kind == "num":
            if not re.search(r"[.eE]", lit.value):
                raise ModelError("OutOfRange")
            f = parse_number(lit.value)
            if f != math.floor(f):
                raise ModelError("InvalidValue")
            if not (-(2.0**63) <= f < 2.0**63):
                raise ModelError("OutOfRange")
            return int(f)
        raise ModelError("TypeMismatch")
    if lit.kind != "str":
        raise ModelError("TypeMismatch")
    if col.kind == "str":
        if len(lit.value) > col.param:
            raise ModelError("DataTooLong")
        return lit.value
    v = parse_vector(lit.value)
    if len(v) != col.param:
        raise ModelError("VectorDim")
    return v


# ---------------------------------------------------------------- expressions
#
# Operand: ("col", name) | ("lit", Lit) | ("dist", colname, vector_text)
# Condition: list of (left, op, right) conjuncts joined by AND.


def render_operand(o):
    if o[0] == "col":
        return o[1]
    if o[0] == "lit":
        return o[1].sql()
    return f"L2_DISTANCE({o[1]}, {Lit('str', o[2]).sql()})"


def compile_operand(t, o):
    """Returns (kind, evaluator)."""
    if o[0] == "col":
        i = t.col(o[1])
        kind = {"int": "num", "str": "str", "vec": "vec"}[t.cols[i].kind]
        return kind, lambda row: row[i]
    if o[0] == "lit":
        lit = o[1]
        if lit.kind == "int":
            return "num", lambda row: lit.value
        if lit.kind == "num":
            v = parse_number(lit.value)
            return "num", lambda row: v
        return "str", lambda row: lit.value
    i = t.col(o[1])
    if t.cols[i].kind != "vec":
        raise ModelError("TypeMismatch")
    q = parse_vector(o[2])
    if len(q) != t.cols[i].param:
        raise ModelError("VectorDim")
    return "num", lambda row: l2(row[i], q)


OPS = {
    "=": lambda a, b: a == b,
    "<>": lambda a, b: a != b,
    "<": lambda a, b: a < b,
    "<=": lambda a, b: a <= b,
    ">": lambda a, b: a > b,
    ">=": lambda a, b: a >= b,
}


def compile_cond(t, cond):
    tests = []
    for left, op, right in cond or []:
        lk, lf = compile_operand(t, left)
        rk, rf = compile_operand(t, right)
        if not ((lk == rk == "num") or (lk == rk == "str")):
            raise ModelError("TypeMismatch")
        fn = OPS[op]
        tests.append(lambda row, lf=lf, rf=rf, fn=fn: fn(lf(row), rf(row)))
    return lambda row: all(f(row) for f in tests)


def render_cond(cond):
    return " AND ".join(f"{render_operand(l)} {op} {render_operand(r)}" for l, op, r in cond)


# ---------------------------------------------------------------- statements


class Stmt:
    def __init__(self, kind, **kw):
        self.kind = kind
        self.__dict__.update(kw)

    def sql(self):
        k = self.kind
        if k in ("begin", "commit", "rollback"):
            return k.upper()
        if k == "raw":
            return self.text
        if k == "create":
            cols = []
            for i, c in enumerate(self.cols):
                s = f"{c.name} {c.sql_type()}"
                if i == self.pk and not self.table_pk:
                    s += " PRIMARY KEY"
                cols.append(s)
            if self.table_pk:
                cols.append(f"PRIMARY KEY ({self.cols[self.pk].name})")
            ine = "IF NOT EXISTS " if self.if_not_exists else ""
            return f"CREATE TABLE {ine}{self.table} ({', '.join(cols)})"
        if k == "drop":
            return f"DROP TABLE {'IF EXISTS ' if self.if_exists else ''}{self.table}"
        if k == "index":
            return f"CREATE VECTOR INDEX {self.name} ON {self.table}({self.col})"
        if k == "insert":
            cols = f" ({', '.join(self.cols)})" if self.cols is not None else ""
            rows = ", ".join("(" + ", ".join(l.sql() for l in r) + ")" for r in self.rows)
            return f"INSERT INTO {self.table}{cols} VALUES {rows}"
        if k == "update":
            sets = ", ".join(f"{c} = {l.sql()}" for c, l in self.sets)
            where = f" WHERE {render_cond(self.where)}" if self.where else ""
            return f"UPDATE {self.table} SET {sets}{where}"
        if k == "delete":
            where = f" WHERE {render_cond(self.where)}" if self.where else ""
            return f"DELETE FROM {self.table}{where}"
        if k == "select":
            items = []
            for it in self.items:
                if it[0] == "star":
                    items.append("*")
                else:
                    s = render_operand(it[1])
                    items.append(s + (f" AS {it[2]}" if it[2] else ""))
            s = f"SELECT {', '.join(items)} FROM {self.table}"
            if self.where:
                s += f" WHERE {render_cond(self.where)}"
            if self.order:
                parts = []
                for ref, desc in self.order:
                    r = ref[1] if ref[0] in ("alias", "col") else str(ref[1]) if ref[0] == "pos" else render_operand(ref)
                    parts.append(r + (" DESC" if desc else ""))
                s += " ORDER BY " + ", ".join(parts)
            if self.limit is not None:
                s += f" LIMIT {self.limit}"
                if self.offset is not None:
                    s += f" OFFSET {self.offset}"
            return s
        raise AssertionError(k)


# ---------------------------------------------------------------- model


# Rules the task text leaves open, set from --nested-begin and --update-count
# for the model and, through the environment, for the database it checks
# (Semantics in src/engine/mod.rs).
NESTED_BEGIN_IS_ERROR = True
UPDATE_COUNTS_MATCHED = True
# Dimensions of generated vector columns (--dims); above 64 the 8-bit scan
# stops early for rows already out of reach.
VEC_DIMS = (1, 4)
# Vector components written as the benchmark writes them, `-1.750000`, which
# the engine parses on its eight-byte path (--six-decimals); same values.
SIX_DECIMALS = False
# One component in eight beyond the 24-bit codes' 8.388607 (--wide), so a
# vector column holds rows of both encodings; still quarters, so distances
# stay exact.
WIDE = False


class Model:
    def __init__(self):
        self.db = {}
        self.work = {}
        self.in_txn = False

    def clone(self):
        m = Model()
        m.db, m.work, m.in_txn = clone_db(self.db), clone_db(self.work), self.in_txn
        return m

    def commit(self):
        self.db = clone_db(self.work)

    def restart(self):
        self.work = clone_db(self.db)
        self.in_txn = False

    def table(self, s, name):
        t = s.get(name.lower())
        if t is None:
            raise ModelError("NoSuchTable")
        return t

    def execute(self, st):
        try:
            return self._execute(st)
        except ModelError as e:
            return {"error": e.kind}

    def _execute(self, st):
        k = st.kind
        if k == "begin":
            if self.in_txn and NESTED_BEGIN_IS_ERROR:
                raise ModelError("Unsupported")
            self.commit()
            self.in_txn = True
            return {"ok": True, "affected": 0}
        if k == "commit":
            self.commit()
            self.in_txn = False
            return {"ok": True, "affected": 0}
        if k == "rollback":
            self.work = clone_db(self.db)
            self.in_txn = False
            return {"ok": True, "affected": 0}
        if k == "raw":
            if st.ddl:
                # Parsed DDL commits the open transaction before failing.
                self.commit()
                self.in_txn = False
            raise ModelError(st.expect)
        if k in ("create", "drop", "index"):
            self.commit()
            self.in_txn = False
            self.ddl(st)
            self.work = clone_db(self.db)
            return {"ok": True, "affected": 0}
        if k in ("insert", "update", "delete"):
            scratch = clone_db(self.work)
            n = getattr(self, k)(scratch, st)
            self.work = scratch
            if not self.in_txn:
                self.commit()
            return {"ok": True, "affected": n}
        if k == "select":
            return self.select(self.work, st)
        raise AssertionError(k)

    def ddl(self, st):
        db = self.db
        if st.kind == "create":
            if st.table.lower() in db:
                if st.if_not_exists:
                    return
                raise ModelError("TableExists")
            db[st.table.lower()] = Table(st.table, st.cols, st.pk)
        elif st.kind == "drop":
            if st.table.lower() not in db:
                if st.if_exists:
                    return
                raise ModelError("NoSuchTable")
            del db[st.table.lower()]
        else:
            t = self.table(db, st.table)
            i = t.col(st.col)
            if t.cols[i].kind != "vec" or t.index is not None:
                raise ModelError("Schema")
            t.index = st.name

    def insert(self, s, st):
        t = self.table(s, st.table)
        if st.cols is not None:
            pos = []
            for c in st.cols:
                i = t.col(c)
                if i in pos:
                    raise ModelError("Schema")
                pos.append(i)
            if any(i not in pos for i in range(len(t.cols))):
                raise ModelError("NoDefault")
        else:
            pos = list(range(len(t.cols)))
        if any(len(r) != len(pos) for r in st.rows):
            raise ModelError("ColumnCount")
        for r in st.rows:
            vals = [None] * len(t.cols)
            for lit, i in zip(r, pos):
                vals[i] = coerce(lit, t.cols[i])
            pk = vals[t.pk]
            if pk in t.rows:
                raise ModelError("DuplicateKey")
            t.rows[pk] = tuple(vals)
        return len(st.rows)

    def update(self, s, st):
        t = self.table(s, st.table)
        sets = []
        for c, lit in st.sets:
            i = t.col(c)
            if i == t.pk:
                raise ModelError("Unsupported")
            sets.append((i, coerce(lit, t.cols[i])))
        pred = compile_cond(t, st.where)
        matched = changed = 0
        for pk in sorted(t.rows):
            row = t.rows[pk]
            if not pred(row):
                continue
            matched += 1
            new = list(row)
            for i, v in sets:
                new[i] = v
            new = tuple(new)
            if new != row:
                t.rows[pk] = new
                changed += 1
        return matched if UPDATE_COUNTS_MATCHED else changed

    def delete(self, s, st):
        t = self.table(s, st.table)
        pred = compile_cond(t, st.where)
        doomed = [pk for pk in t.rows if pred(t.rows[pk])]
        for pk in doomed:
            del t.rows[pk]
        return len(doomed)

    def select(self, s, st):
        t = self.table(s, st.table)
        names, outs, kinds, aliases = [], [], [], {}
        for it in st.items:
            if it[0] == "star":
                for i, c in enumerate(t.cols):
                    names.append(c.name)
                    outs.append(lambda row, i=i: row[i])
                    kinds.append({"int": "num", "str": "str", "vec": "vec"}[c.kind])
                continue
            kind, fn = compile_operand(t, it[1])
            if it[2]:
                aliases[it[2].lower()] = len(outs)
            names.append(it[2] or (it[1][1] if it[1][0] == "col" else render_operand(it[1])))
            outs.append(fn)
            kinds.append(kind)
        pred = compile_cond(t, st.where)
        keys = []
        for ref, desc in st.order or []:
            if ref[0] in ("alias", "col") and ref[1].lower() in aliases:
                fn = outs[aliases[ref[1].lower()]]
                kind = kinds[aliases[ref[1].lower()]]
            elif ref[0] == "pos":
                if not 1 <= ref[1] <= len(outs):
                    raise ModelError("NoSuchColumn")
                fn = outs[ref[1] - 1]
                kind = kinds[ref[1] - 1]
            elif ref[0] == "alias":
                raise ModelError("NoSuchColumn")
            else:
                kind, fn = compile_operand(t, ref)
            if kind == "vec":
                raise ModelError("Unsupported")
            keys.append((fn, desc))
        rows = [t.rows[pk] for pk in sorted(t.rows) if pred(t.rows[pk])]
        if keys:
            rows.sort(key=lambda r: r[t.pk])
            for fn, desc in reversed(keys):
                rows.sort(key=fn, reverse=desc)
        off = st.offset or 0
        rows = rows[off:] if st.limit is None else rows[off : off + st.limit]
        return {"columns": names, "rows": [[fn(r) for fn in outs] for r in rows]}


# ---------------------------------------------------------------- generator


class Gen:
    NAMES = ["t0", "t1", "T2"]

    def __init__(self, rng, model):
        self.rng, self.model = rng, model

    def r(self):
        return self.rng

    def comp(self):
        k = self.rng.randint(-8, 8)
        if WIDE and self.rng.random() < 0.125:
            k = self.rng.choice([-1, 1]) * self.rng.randint(34, 48)
        if SIX_DECIMALS:
            return f"{k / 4:.6f}"
        return "0" if k == 0 else f"{k / 4:g}"

    def vec_text(self, dim):
        roll = self.rng.random()
        if roll < 0.03:
            dim = max(1, dim + self.rng.choice([-1, 1]))
        elif roll < 0.05:
            return self.rng.choice(["[nan, 1]", "[1e39]", "[1,,2]", "abc", "[]", "[inf]", "[-1e40, 0]"])
        sep = self.rng.choice([",", ", ", " , "])
        return "[" + sep.join(self.comp() for _ in range(dim)) + "]"

    def pk_lit(self):
        roll = self.rng.random()
        if roll < 0.03:
            return Lit("int", self.rng.choice([I64_MIN, I64_MAX, 0]))
        if roll < 0.05:
            return Lit("num", self.rng.choice(["3.0", "2.5", "1e1", "99999999999999999999"]))
        return Lit("int", self.rng.randint(-3, 30))

    STRS = ["a", "b", "c", "ab", "é", "中文", "", "it's", "a b", "zz", "\\n"]

    def str_lit(self, maxlen):
        if self.rng.random() < 0.05:
            return Lit("str", self.rng.choice(["x" * maxlen, "y" * (maxlen + 1), "长" * (maxlen + 1)]))
        return Lit("str", self.rng.choice([s for s in self.STRS if len(s) <= maxlen]))

    def int_lit(self):
        return Lit("int", self.rng.randint(-5, 5))

    def value_for(self, c):
        if self.rng.random() < 0.02:
            return self.rng.choice([Lit("int", 1), Lit("str", "1"), Lit("num", "0.5")])
        if c.kind == "int":
            return self.int_lit() if c.name != "id" else self.pk_lit()
        if c.kind == "str":
            return self.str_lit(c.param)
        return Lit("str", self.vec_text(c.param))

    def tables(self):
        return list(self.model.work.values())

    def pick_table(self):
        ts = self.tables()
        if not ts or self.rng.random() < 0.03:
            return None
        return self.rng.choice(ts)

    def create(self):
        name = self.rng.choice(self.NAMES)
        cols = [Col("id", "int", 0), Col("cat", "str", self.rng.randint(2, 5))]
        if self.rng.random() < 0.7:
            cols.append(Col("num", "int", 0))
        if self.rng.random() < 0.85:
            cols.append(Col("v", "vec", self.rng.randint(*VEC_DIMS)))
        if self.rng.random() < 0.3:
            cols.append(Col("note", "str", 8))
        self.rng.shuffle(cols)
        pk = next(i for i, c in enumerate(cols) if c.name == "id")
        return Stmt("create", table=name, cols=cols, pk=pk, table_pk=self.rng.random() < 0.3,
                    if_not_exists=self.rng.random() < 0.2)

    def operand_for(self, t, c):
        return ("col", c.name)

    def conjunct(self, t):
        rng = self.rng
        scalar = [c for c in t.cols if c.kind != "vec"]
        if t.vec is not None and rng.random() < 0.1:
            c = t.cols[t.vec]
            return (("dist", c.name, self.vec_text(c.param)), rng.choice(["<", "<=", ">"]), ("lit", Lit("num", rng.choice(["0.5", "1.25", "2"]))))
        c = rng.choice(scalar)
        if rng.random() < 0.03:
            c = rng.choice(t.cols)
        op = rng.choice(list(OPS))
        if c.kind == "int":
            lit = self.pk_lit() if c.name == "id" else self.int_lit()
            if rng.random() < 0.05:
                lit = Lit("num", rng.choice(["1.5", "-0.5", "3"]))
        elif c.kind == "str":
            lit = self.str_lit(c.param)
        else:
            lit = Lit("str", self.vec_text(c.param))
        if rng.random() < 0.03:
            lit = rng.choice([Lit("str", "1"), Lit("int", 1)])
        left, right = ("col", c.name), ("lit", lit)
        if rng.random() < 0.2:
            left, right = right, left
            op = {"<": ">", ">": "<", "<=": ">=", ">=": "<="}.get(op, op)
        return (left, op, right)

    def where(self, t):
        n = self.rng.choice([0, 0, 1, 1, 1, 2])
        return [self.conjunct(t) for _ in range(n)]

    def insert(self, t):
        rng = self.rng
        cols = None
        order = list(range(len(t.cols)))
        if rng.random() < 0.3:
            rng.shuffle(order)
            cols = [t.cols[i].name for i in order]
            if rng.random() < 0.05:
                cols = cols[:-1]
            if rng.random() < 0.03:
                cols = cols + ["nosuch"]
        rows = []
        for _ in range(rng.choice([1, 1, 1, 2, 3, 5])):
            row = [self.value_for(t.cols[i]) for i in order]
            if rng.random() < 0.02:
                row = row[:-1]
            rows.append(row)
        return Stmt("insert", table=t.name, cols=cols, rows=rows)

    def update(self, t):
        rng = self.rng
        settable = [c for i, c in enumerate(t.cols) if i != t.pk]
        if not settable or rng.random() < 0.03:
            sets = [("id", self.pk_lit())]
        else:
            sets = [(c.name, self.value_for(c)) for c in rng.sample(settable, rng.randint(1, len(settable)))]
        return Stmt("update", table=t.name, sets=sets, where=self.where(t))

    def select(self, t):
        rng = self.rng
        items, aliases = [], []
        if rng.random() < 0.4:
            items.append(("star",))
        else:
            for c in rng.sample(t.cols, rng.randint(1, len(t.cols))):
                alias = rng.choice([None, None, None, "x_" + c.name])
                items.append(("item", ("col", c.name), alias))
                if alias:
                    aliases.append(alias)
        if t.vec is not None and rng.random() < 0.3:
            c = t.cols[t.vec]
            items.append(("item", ("dist", c.name, self.vec_text(c.param)), "d"))
            aliases.append("d")
        order = []
        for _ in range(rng.choice([0, 0, 1, 1, 2])):
            roll = rng.random()
            if roll < 0.3 and aliases:
                ref = ("alias", rng.choice(aliases))
            elif roll < 0.4:
                ref = ("pos", rng.randint(1, 3))
            else:
                ref = ("col", rng.choice(t.cols).name)
            order.append((ref, rng.random() < 0.3))
        limit = rng.choice([None, None, 0, 1, 2, 3, 5])
        offset = rng.choice([None, None, 1, 2]) if limit is not None else None
        return Stmt("select", table=t.name, items=items, where=self.where(t), order=order, limit=limit, offset=offset)

    def topk(self, t):
        rng = self.rng
        c = t.cols[t.vec]
        q = self.vec_text(c.param)
        items = [("item", ("col", "id"), None), ("item", ("dist", c.name, q), "distance")]
        if rng.random() < 0.3:
            items.insert(1, ("item", ("col", "cat"), None))
        where = []
        if rng.random() < 0.5:
            cat = next(x for x in t.cols if x.name == "cat")
            where = [(("col", "cat"), "=", ("lit", self.str_lit(cat.param)))]
        if rng.random() < 0.2:
            where.append(self.conjunct(t))
        ref = ("alias", "distance") if rng.random() < 0.7 else ("dist", c.name, q)
        return Stmt("select", table=t.name, items=items, where=where, order=[(ref, False)],
                    limit=rng.randint(1, 6), offset=rng.choice([None, None, 1]))

    # (sql, expected error, parses as DDL and so commits first)
    RAW = [
        ("SELECT * FROM t0 WHERE id = 1 OR id = 2", "Unsupported", False),
        ("SELEC * FROM t0", "Syntax", False),
        ("SELECT COUNT(*) FROM t0", "Unsupported", False),
        ("SELECT id FROM t0 GROUP BY id", "Unsupported", False),
        ("CREATE TABLE bad (a INT PRIMARY KEY)", "Unsupported", False),
        ("CREATE TABLE bad (a BIGINT, v VECTOR(2))", "Schema", True),
        ("CREATE TABLE bad (a VARCHAR(3) PRIMARY KEY)", "Schema", True),
        ("CREATE TABLE bad (a BIGINT PRIMARY KEY, v VECTOR(2), w VECTOR(3))", "Schema", True),
        ("SELECT * FROM nosuch", "NoSuchTable", False),
        ("INSERT INTO t0 VALUES (NULL)", "Unsupported", False),
        ("ALTER TABLE t0 ADD COLUMN z BIGINT", "Unsupported", False),
        ("SELECT * FROM t0 JOIN t1", "Unsupported", False),
    ]

    def next(self):
        rng = self.rng
        roll = rng.random()
        t = self.pick_table()
        if not self.tables() or roll < 0.03:
            return self.create()
        if roll < 0.036:
            name = rng.choice(self.NAMES + ["nosuch"])
            return Stmt("drop", table=name, if_exists=rng.random() < 0.5)
        if roll < 0.06:
            text, kind, ddl = rng.choice(self.RAW)
            return Stmt("raw", text=text, expect=kind, ddl=ddl)
        if roll < 0.07 and t is not None:
            col = t.cols[t.vec].name if t.vec is not None and rng.random() < 0.8 else rng.choice(t.cols).name
            return Stmt("index", name=f"ix{rng.randint(0, 3)}", table=t.name, col=col)
        if roll < 0.13:
            return Stmt(rng.choice(["begin", "begin", "commit", "rollback"]))
        if t is None:
            return Stmt("select", table="nosuch", items=[("star",)], where=[], order=[], limit=None, offset=None)
        if roll < 0.43:
            return self.insert(t)
        if roll < 0.53:
            return self.update(t)
        if roll < 0.60:
            return Stmt("delete", table=t.name, where=self.where(t))
        if roll < 0.75 and t.vec is not None and any(c.name == "cat" for c in t.cols):
            return self.topk(t)
        return self.select(t)


# ---------------------------------------------------------------- database process


class Crashed(Exception):
    pass


# The database reports semantic error names (ErrorKind::name); the model
# uses the kind names.
ERROR_KINDS = {
    "syntax_error": "Syntax", "unsupported_sql": "Unsupported", "table_exists": "TableExists",
    "table_not_found": "NoSuchTable", "column_not_found": "NoSuchColumn", "duplicate_key": "DuplicateKey",
    "column_count_mismatch": "ColumnCount", "missing_value": "NoDefault", "type_mismatch": "TypeMismatch",
    "out_of_range": "OutOfRange", "data_too_long": "DataTooLong", "dimension_mismatch": "VectorDim",
    "invalid_value": "InvalidValue", "invalid_schema": "Schema", "transaction_state": "TxnState",
    "io_error": "Io", "corrupt_data": "Corrupt", "internal_error": "Internal",
}


class Db:
    """The database behind the judge's protocol (STDIO_PROTOCOL.md): one JSON
    request per line, one JSON response per request. Results are handed on
    in the model's shape: {"ok", "affected"}, {"columns", "rows"} or
    {"error"}."""

    def __init__(self, binary, data_dir, log_path, timeout, wait_ready=True):
        self.timeout = timeout
        self.log = open(log_path, "ab")
        self.seq = 0
        self.p = subprocess.Popen(
            [binary, "--data-dir", data_dir],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
        )
        if not wait_ready:
            return
        ready = self._readline()
        if json.loads(ready) != {"type": "ready"}:
            raise Crashed(f"bad ready line {ready!r}")

    def _readline(self):
        timer = threading.Timer(self.timeout, self.p.kill)
        timer.start()
        try:
            line = self.p.stdout.readline()
        finally:
            timer.cancel()
        if not line:
            raise Crashed(f"no output (exit code {self.p.poll()})")
        return line.decode()

    def _request(self, **fields):
        self.seq += 1
        line = json.dumps({"id": self.seq, **fields}, ensure_ascii=False) + "\n"
        self.p.stdin.write(line.encode())
        self.p.stdin.flush()

    def _response(self):
        r = json.loads(self._readline())
        if r.get("id") != self.seq or type(r.get("ok")) is not bool:
            raise Crashed(f"response {r} to request {self.seq}")
        return r

    def send(self, sql):
        self._request(op="execute", sql=sql + ";", search_mode="exact")

    def query(self, sql):
        self.send(sql)
        r = self._response()
        if not r["ok"]:
            code = r["error"]["code"]
            return {"error": ERROR_KINDS.get(code, code), "message": r["error"].get("message")}
        if "columns" in r:
            return {"columns": r["columns"], "rows": r["rows"]}
        return {"ok": True, "affected": r.get("affected_rows", 0)}

    def kill(self):
        self.p.kill()
        self.p.wait()
        self.log.close()

    def close(self):
        """A normal shutdown as the judge does it: the acknowledgement, then
        nothing more on stdout and exit status 0."""
        try:
            self._request(op="shutdown")
            ack = self._response()
            self.p.stdin.close()
        except BrokenPipeError:
            ack = None
        extra = self.p.stdout.read()
        code = self.p.wait(timeout=self.timeout)
        self.log.close()
        if ack != {"id": self.seq, "ok": True} or extra or code != 0:
            raise Crashed(f"shutdown: answer {ack}, then {extra!r}, exit code {code}")


# ---------------------------------------------------------------- comparison


def same_cell(a, b):
    if isinstance(a, (list, tuple)) and isinstance(b, (list, tuple)):
        return len(a) == len(b) and all(same_cell(x, y) for x, y in zip(a, b))
    if isinstance(a, bool) or isinstance(b, bool):
        return a == b
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        if isinstance(a, int) and isinstance(b, int):
            return a == b
        return math.isclose(a, b, rel_tol=1e-9, abs_tol=1e-9)
    return a == b


def same_result(exp, got):
    if "error" in exp:
        return got.get("error") == exp["error"]
    if "ok" in exp:
        return got.get("ok") is True and got.get("affected") == exp["affected"]
    if got.get("columns") != exp["columns"]:
        return False
    rows = got.get("rows")
    return rows is not None and len(rows) == len(exp["rows"]) and all(
        same_cell(a, b) for a, b in zip(rows, exp["rows"])
    )


def snapshot(db_proc, names):
    """Actual table contents: name -> sorted rows, or None if missing."""
    out = {}
    for name in names:
        r = db_proc.query(f"SELECT * FROM {name} ORDER BY id")
        if "error" in r:
            if r["error"] != "NoSuchTable":
                raise Crashed(f"snapshot of {name} failed: {r}")
            out[name] = None
        else:
            out[name] = r["rows"]
    return out


def model_snapshot(db, names):
    return {n: ([list(t.rows[pk]) for pk in sorted(t.rows)] if (t := db.get(n)) else None) for n in names}


def index_state(model):
    return {n: t.index for n, t in model.db.items()}


def same_snapshot(a, b):
    return all(
        (a[n] is None) == (b[n] is None)
        and (a[n] is None or (len(a[n]) == len(b[n]) and all(same_cell(x, y) for x, y in zip(a[n], b[n]))))
        for n in a
    )


# ---------------------------------------------------------------- driver


def fmt(o):
    return json.dumps(o, ensure_ascii=False)[:600]


def run_seed(args, seed):
    rng = random.Random(seed)
    model = Model()
    gen = Gen(rng, model)
    work = tempfile.mkdtemp(prefix=f"difftest-{seed}-", dir=args.workdir)
    data = os.path.join(work, "data")
    log_path = os.path.join(work, "stderr.log")
    history = []
    names = [n.lower() for n in Gen.NAMES]

    def fail(msg):
        repro = os.path.join(work, "repro.sql")
        with open(repro, "w") as f:
            f.write("".join(s + ";\n" for s in history))
        print(f"seed {seed} FAILED: {msg}\n  statements: {repro}\n  stderr: {log_path}")
        return False

    db = Db(args.bin, data, log_path, args.timeout)
    try:
        for step in range(args.steps):
            st = gen.next()
            sql = st.sql()
            history.append(sql)
            roll = rng.random()
            if args.crash and roll < 0.04:
                # Kill while the statement may be executing: either outcome
                # of a commit is acceptable, a partial one is not.
                before = model_snapshot(model.db, names)
                after_model = model.clone()
                after_model.execute(st)
                after = model_snapshot(after_model.db, names)
                mid = None
                if st.kind in ("create", "drop"):
                    # DDL first commits the open transaction in its own log
                    # frame, so a crash can keep that commit without the DDL.
                    mid_model = model.clone()
                    mid_model.commit()
                    mid = model_snapshot(mid_model.db, names)
                db.send(sql)
                # A commit takes well under a millisecond here, so most kills
                # must come almost immediately to land before it is durable.
                delay = rng.choice([0, 0, rng.random() * 0.0003, rng.random() * 0.002])
                if delay:
                    time.sleep(delay)
                db.kill()
                if rng.random() < 0.3:
                    # Kill again while recovery may still be running.
                    db = Db(args.bin, data, log_path, args.timeout, wait_ready=False)
                    time.sleep(rng.random() * 0.002)
                    db.kill()
                db = Db(args.bin, data, log_path, args.timeout)
                got = snapshot(db, names)
                changes = before != after
                # A vector index changes no rows, so the snapshot cannot tell
                # whether a CREATE VECTOR INDEX survived the crash.
                hidden = st.kind == "index" and index_state(model) != index_state(after_model)
                if same_snapshot(got, after) and not hidden:
                    model = after_model
                    STATS[f"crash:{'committed' if changes else 'no-op'}"] += 1
                elif same_snapshot(got, before) and not hidden:
                    STATS["crash:lost-uncommitted"] += 1
                elif hidden and (same_snapshot(got, after) or same_snapshot(got, before)):
                    # Repeating the statement fails only if the index exists;
                    # the model then runs the same probe to stay in step. An
                    # open transaction is committed in its own log frame
                    # before the DDL, so its rows can survive without the index.
                    probe = db.query(sql)
                    survived = probe.get("error") == "Schema"
                    if same_snapshot(got, after):
                        model = after_model
                        if not survived:
                            model.table(model.db, st.table).index = None
                    STATS[f"crash:index-{'committed' if survived else 'lost'}"] += 1
                    model.restart()
                    want = model.execute(st)
                    history.append(f"{sql};  -- probe: did the index survive the crash?")
                    if not same_result(probe, want):
                        return fail(f"step {step}: index probe after crash during `{sql}`\n"
                                    f"  expected: {fmt(want)}\n  got:      {fmt(probe)}")
                elif mid is not None and same_snapshot(got, mid):
                    model = mid_model
                    STATS["crash:ddl-commit-only"] += 1
                else:
                    return fail(f"step {step}: state after crash during `{sql}` matches neither side\n"
                                f"  got:    {fmt(got)}\n  before: {fmt(before)}\n  after:  {fmt(after)}")
                model.restart()
                gen.model = model
                history.append("-- crash during previous statement; restart")
                continue
            exp = model.execute(st)
            STATS[st.kind + ":" + ("error:" + exp["error"] if "error" in exp else
                                   f"rows={min(len(exp['rows']), 3)}" if "rows" in exp else "ok")] += 1
            got = db.query(sql)
            if not same_result(exp, got):
                return fail(f"step {step}: `{sql}`\n  expected: {fmt(exp)}\n  got:      {fmt(got)}")
            if roll > 1 - args.restart_rate:
                if args.crash and rng.random() < 0.5:
                    db.kill()
                    history.append("-- kill -9; restart")
                else:
                    db.close()
                    history.append("-- clean restart")
                db = Db(args.bin, data, log_path, args.timeout)
                model.restart()
                got = snapshot(db, names)
                want = model_snapshot(model.db, names)
                if not same_snapshot(got, want):
                    return fail(f"step {step}: state after restart differs\n  got:  {fmt(got)}\n  want: {fmt(want)}")
        db.close()
    except Crashed as e:
        return fail(f"database died: {e}")
    with open(log_path, "rb") as f:
        if b"panicked" in f.read():
            return fail("a panic was logged")
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)
    return True


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default="target/release/minivectordb")
    ap.add_argument("--seeds", default="0:20", help="range start:end")
    ap.add_argument("--steps", type=int, default=300)
    ap.add_argument("--crash", action="store_true", help="also kill -9 at random points")
    ap.add_argument("--restart-rate", type=float, default=0.02)
    ap.add_argument("--timeout", type=float, default=20.0)
    ap.add_argument("--workdir", default=None)
    ap.add_argument("--keep", action="store_true", help="keep data dirs of passing seeds")
    ap.add_argument("--stats", action="store_true", help="print how often each outcome was checked")
    ap.add_argument("--dims", default="1:4", help="vector columns get LO to HI dimensions (LO:HI, inclusive)")
    ap.add_argument("--six-decimals", action="store_true", help="write vector components as -1.750000 instead of -1.75")
    ap.add_argument("--wide", action="store_true", help="one vector component in eight between 8.5 and 12 in magnitude")
    ap.add_argument("--nested-begin", choices=["error", "commit"], default="error",
                    help="BEGIN inside a transaction: an error that keeps it open, or an implicit commit")
    ap.add_argument("--update-count", choices=["matched", "changed"], default="matched",
                    help="UPDATE reports the rows its WHERE matched, or only those whose values changed")
    args = ap.parse_args()
    global NESTED_BEGIN_IS_ERROR, UPDATE_COUNTS_MATCHED, VEC_DIMS, SIX_DECIMALS, WIDE
    SIX_DECIMALS = args.six_decimals
    WIDE = args.wide
    VEC_DIMS = tuple(int(x) for x in args.dims.split(":"))
    NESTED_BEGIN_IS_ERROR = args.nested_begin == "error"
    UPDATE_COUNTS_MATCHED = args.update_count == "matched"
    os.environ.update(MVDB_NESTED_BEGIN=args.nested_begin, MVDB_UPDATE_COUNT=args.update_count)
    lo, hi = (int(x) for x in args.seeds.split(":"))
    failed = [s for s in range(lo, hi) if not run_seed(args, s)]
    if args.stats:
        for k, n in sorted(STATS.items()):
            print(f"  {n:6d}  {k}")
    print(f"{hi - lo - len(failed)}/{hi - lo} seeds passed" + (f"; failed: {failed}" if failed else ""))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
