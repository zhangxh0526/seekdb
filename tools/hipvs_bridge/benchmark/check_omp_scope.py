"""Exercise real GPU builds and verify caller/default OpenMP settings survive them."""
import argparse
import ctypes as c
from pathlib import Path
import threading

import numpy as np

p = argparse.ArgumentParser()
p.add_argument("--bridge", type=Path, required=True)
p.add_argument("--data", type=Path, required=True)
a = p.parse_args()
data = np.load(a.data / "data.npz")
base = np.ascontiguousarray(data["base"], dtype=np.float32)
query = np.ascontiguousarray(data["queries"][0], dtype=np.float32)
bridge = c.CDLL(str(a.bridge.resolve()))
bridge.seekdb_cuvs_build.argtypes = [c.c_void_p, c.c_long, c.c_long]
bridge.seekdb_cuvs_build.restype = c.c_void_p
bridge.seekdb_cuvs_search.argtypes = [c.c_void_p, c.c_void_p, c.c_long, c.c_long, c.c_void_p, c.c_void_p]
bridge.seekdb_cuvs_free.argtypes = [c.c_void_p]
omp = c.CDLL("/opt/rocm/lib/llvm/lib/libomp.so")
omp.omp_set_num_threads.argtypes = [c.c_int]
omp.kmp_set_defaults.argtypes = [c.c_char_p]
initial_caller = omp.omp_get_max_threads()
threading.stack_size(64 << 20)


def read_default():
    result = []
    thread = threading.Thread(target=lambda: result.append(omp.omp_get_max_threads()))
    thread.start()
    thread.join()
    return result[0]


initial_default = read_default()
barrier = threading.Barrier(2)
errors = []
observed = []


def worker(limit):
    handle = None
    try:
        omp.omp_set_num_threads(limit)
        barrier.wait(timeout=30)
        handle = bridge.seekdb_cuvs_build(base.ctypes.data, *base.shape)
        if not handle:
            raise AssertionError("GPU build failed")
        after = omp.omp_get_max_threads()
        if after != limit:
            raise AssertionError(f"caller thread setting changed: {limit} -> {after}")
        offsets = np.empty(10, dtype=np.uint32)
        rc = bridge.seekdb_cuvs_search(handle, query.ctypes.data, 1, 10, offsets.ctypes.data, None)
        if rc or len(set(map(int, offsets))) != 10 or np.any(offsets >= len(base)):
            raise AssertionError("GPU search returned invalid row offsets")
        observed.append((limit, after))
    except Exception as exc:
        errors.append(repr(exc))
    finally:
        if handle:
            bridge.seekdb_cuvs_free(handle)


try:
    omp.kmp_set_defaults(b"OMP_NUM_THREADS=12")
    threads = [threading.Thread(target=worker, args=(limit,)) for limit in (4, 6)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    if errors:
        raise AssertionError(errors)
    if sorted(observed) != [(4, 4), (6, 6)] or read_default() != 12:
        raise AssertionError(f"OpenMP configuration was not restored: {observed}, default={read_default()}")
    print("PASS: concurrent real GPU builds preserve both caller limits and the new-thread default")
finally:
    omp.kmp_set_defaults(f"OMP_NUM_THREADS={initial_default}".encode())
    omp.omp_set_num_threads(initial_caller)
