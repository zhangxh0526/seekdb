#!/usr/bin/env python3
"""Prepare public SIFT subset or a clearly labelled synthetic development proxy."""
import argparse
import hashlib
import json
import os
from pathlib import Path

os.environ.setdefault("OPENBLAS_NUM_THREADS", "8")
os.environ.setdefault("OMP_NUM_THREADS", "8")
import numpy as np


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--sift-hdf5", type=Path)
    p.add_argument("--input-npz", type=Path)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--rows", type=int, default=250000)
    p.add_argument("--queries", type=int, default=4096)
    p.add_argument("--dim", type=int, default=256)
    a = p.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    if a.sift_hdf5:
        import h5py
        with h5py.File(a.sift_hdf5, "r") as f:
            base = np.asarray(f["train"][:a.rows], dtype=np.float32)
            queries = np.asarray(f["test"][:a.queries], dtype=np.float32)
        source = "Public ANN-Benchmarks SIFT1M, first requested rows/queries; not certified organizer subset"
    elif a.input_npz:
        with np.load(a.input_npz) as f:
            base = np.asarray(f["base"][:a.rows], dtype=np.float32)
            queries = np.asarray(f["queries"][:a.queries], dtype=np.float32)
        source = "Public SIFT development subset transported as NPZ"
    else:
        rng = np.random.default_rng(20261030)
        base = rng.standard_normal((a.rows, a.dim), dtype=np.float32)
        queries = rng.standard_normal((a.queries, a.dim), dtype=np.float32)
        source = "Synthetic standard normal seed 20261030; NOT Hidden250K"
    assert len(base) == a.rows and len(queries) == a.queries and a.rows >= 100
    base, queries = np.ascontiguousarray(base), np.ascontiguousarray(queries)
    ids = np.arange(len(base), dtype=np.int64) * 3 - 700000001
    truth = np.empty((len(queries), 100), dtype=np.int64)
    norms = np.einsum("ij,ij->i", base, base)
    for lo in range(0, len(queries), 32):
        q = queries[lo:lo + 32]
        distances = np.einsum("ij,ij->i", q, q)[:, None] + norms[None, :] - 2 * q @ base.T
        nearest = np.argpartition(distances, 99, axis=1)[:, :100]
        for j in range(len(q)):
            candidate = nearest[j]
            order = np.lexsort((ids[candidate], distances[j, candidate]))
            truth[lo + j] = ids[candidate[order]]
        if lo % 512 == 0:
            print("ground truth", lo, "/", len(queries), flush=True)
    np.savez(a.out / "data.npz", base=base, queries=queries, ids=ids, truth=truth)
    with (a.out / "base.tsv").open("w") as f:
        for ident, row in zip(ids, base):
            f.write(str(int(ident)) + "\t[" + ",".join(format(float(v), ".9g") for v in row) + "]\n")
    meta = {"source": source, "rows": len(base), "queries": len(queries), "dim": base.shape[1],
            "dtype": "FP32", "metric": "L2", "id_formula": "3*i-700000001",
            "truth": "exact FP32 squared L2 via blocked BLAS; deterministic distance/ID ordering within selected Top100",
            "boundary_ties": "Top100 boundary ties may select different equally distant IDs; raw results retained",
            "base_sha256": hashlib.sha256(base.tobytes()).hexdigest(),
            "query_sha256": hashlib.sha256(queries.tobytes()).hexdigest()}
    (a.out / "metadata.json").write_text(json.dumps(meta, indent=2) + "\n")
    print(json.dumps(meta), flush=True)


if __name__ == "__main__":
    main()
