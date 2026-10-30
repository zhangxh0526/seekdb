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
omp.kmp_set_blocktime.argtypes = [c.c_int]
omp.kmp_set_defaults.argtypes = [c.c_char_p]
initial_caller = (omp.omp_get_max_threads(), omp.kmp_get_blocktime())
threading.stack_size(64 << 20)


def read_default():
    result = []
    thread = threading.Thread(target=lambda: result.append((omp.omp_get_max_threads(), omp.kmp_get_blocktime())))
    thread.start()
    thread.join()
    return result[0]


initial_default = read_default()
barrier = threading.Barrier(2)
errors = []
observed = []


def worker(limit, blocktime):
    handle = None
    try:
        omp.omp_set_num_threads(limit)
        omp.kmp_set_blocktime(blocktime)
        barrier.wait(timeout=30)
        handle = bridge.seekdb_cuvs_build(base.ctypes.data, *base.shape)
        if not handle:
            raise AssertionError("GPU build failed")
        after = (omp.omp_get_max_threads(), omp.kmp_get_blocktime())
        if after != (limit, blocktime):
            raise AssertionError(f"caller settings changed: {(limit, blocktime)} -> {after}")
        offsets = np.empty(10, dtype=np.uint32)
        rc = bridge.seekdb_cuvs_search(handle, query.ctypes.data, 1, 10, offsets.ctypes.data, None)
        if rc or len(set(map(int, offsets))) != 10 or np.any(offsets >= len(base)):
            raise AssertionError("GPU search returned invalid row offsets")
        observed.append(((limit, blocktime), after))
    except Exception as exc:
        errors.append(repr(exc))
    finally:
        if handle:
            bridge.seekdb_cuvs_free(handle)


try:
    omp.kmp_set_defaults(b"OMP_NUM_THREADS=12")
    omp.kmp_set_defaults(b"KMP_BLOCKTIME=23")
    threads = [threading.Thread(target=worker, args=limits) for limits in ((4, 3), (6, 7))]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    if errors:
        raise AssertionError(errors)
    if sorted(observed) != [((4, 3), (4, 3)), ((6, 7), (6, 7))] or read_default() != (12, 23):
        raise AssertionError(f"OpenMP configuration was not restored: {observed}, default={read_default()}")
    print("PASS: concurrent real GPU builds preserve caller/default thread limits and wait settings")
finally:
    omp.kmp_set_defaults(f"OMP_NUM_THREADS={initial_default[0]}".encode())
    omp.kmp_set_defaults(f"KMP_BLOCKTIME={initial_default[1]}".encode())
    omp.omp_set_num_threads(initial_caller[0])
    omp.kmp_set_blocktime(initial_caller[1])
