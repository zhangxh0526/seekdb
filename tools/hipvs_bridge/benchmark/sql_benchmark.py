#!/usr/bin/env python3
"""Isolated offline-DDL development protocol. Never reports an official score."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
import time

import numpy as np
import pymysql


def write_json(path, value):
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(value, indent=2) + "\n")
    tmp.replace(path)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--binary", type=Path, required=True)
    p.add_argument("--bridge", type=Path, required=True)
    p.add_argument("--data", type=Path, required=True)
    p.add_argument("--run", type=Path, required=True)
    p.add_argument("--port", type=int, default=2981)
    p.add_argument("--queries", type=int, default=4096)
    p.add_argument("--lib", choices=["cuvs", "vsag"], default="cuvs")
    p.add_argument("--restart", action="store_true")
    p.add_argument("--profile", action="store_true")
    a = p.parse_args()
    a.run.mkdir(parents=True, exist_ok=False)
    data = np.load(a.data / "data.npz")
    queries, truth, ids = data["queries"], data["truth"], data["ids"]
    assert a.queries <= len(queries)
    valid_ids = set(map(int, ids))
    dim = data["base"].shape[1]
    vectors = ["[" + ",".join(format(float(v), ".9g") for v in q) + "]" for q in queries[:a.queries]]
    sqls = {k: [f"SELECT id FROM items ORDER BY l2_distance(v,{v}) APPROXIMATE LIMIT {k}" for v in vectors]
            for k in (10, 100)}
    result = {"official": False, "started_unix": time.time(), "lib": a.lib, "profiled": a.profile,
              "binary": str(a.binary), "bridge": str(a.bridge), "query_count_per_k": a.queries,
              "cpu_count_parameter": 16, "memory_budget": "8G", "data": json.loads((a.data / "metadata.json").read_text())}
    for key, path in [("binary", a.binary), ("bridge", a.bridge)]:
        h = hashlib.sha256()
        with path.open("rb") as f:
            for block in iter(lambda: f.read(8 << 20), b""):
                h.update(block)
        result[key + "_sha256"] = h.hexdigest()
    process = None
    conn = None
    samples = []
    stop_monitor = threading.Event()
    # sysfs can expose host cards not assigned to this container. Monitor only
    # the character devices actually passed through under /dev/dri.
    assigned_devices = set()
    for device in Path("/dev/dri").glob("*"):
        if device.is_char_device():
            st = device.stat()
            assigned_devices.add(Path(f"/sys/dev/char/{os.major(st.st_rdev)}:{os.minor(st.st_rdev)}/device").resolve())
    gpu_files = sorted(p / "mem_info_vram_used" for p in assigned_devices if (p / "mem_info_vram_used").exists())
    result["vram_sample_paths"] = list(map(str, gpu_files))
    result["vram_monitor_available"] = bool(gpu_files)

    def monitor():
        while not stop_monitor.wait(.1):
            s = {"unix": time.time()}
            for f in gpu_files:
                try:
                    s[str(f)] = int(f.read_text())
                except OSError:
                    pass
            try:
                s["cgroup_memory_bytes"] = int(Path("/sys/fs/cgroup/memory.current").read_text())
                s["pid_status"] = [x for x in Path(f"/proc/{process.pid}/status").read_text().splitlines()
                                   if x.startswith(("VmRSS:", "VmHWM:"))]
            except (OSError, AttributeError):
                pass
            samples.append(s)

    def connect():
        return pymysql.connect(host="127.0.0.1", port=a.port, user="root", autocommit=True,
                               local_infile=True, connect_timeout=2, read_timeout=360, write_timeout=360)

    def start(restart=False):
        nonlocal process, conn
        env = os.environ.copy()
        env["LD_LIBRARY_PATH"] = f"{a.bridge.parent}:/opt/hipvs/lib:/opt/rocm/lib"
        env.pop("OB_VSAG_USE_CUVS", None)
        command = [str(a.binary), "--nodaemon", "--port", str(a.port), "--base-dir", str(a.run),
                   "--data-dir", str(a.run / "store"), "--log-level", "INFO",
                   "--parameter", "__min_full_resource_pool_memory=2147483648", "--parameter", "memory_budget=8G",
                   "--parameter", "datafile_size=4G", "--parameter", "cpu_count=16",
                   "--parameter", "local_ip=127.0.0.1"]
        if a.profile:
            command = ["/opt/rocm/bin/rocprofv3", "--hip-trace", "--kernel-trace", "--memory-copy-trace",
                       "--output-directory", str(a.run / ("profile-restart" if restart else "profile")),
                       "--output-format", "csv", "--"] + command
        (a.run / ("restart-command.json" if restart else "start-command.json")).write_text(json.dumps(command, indent=2))
        log = (a.run / ("restart-console.log" if restart else "console.log")).open("w")
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, env=env, start_new_session=True)
        log.close()
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"server exited {process.returncode}; see console log")
            try:
                conn = connect()
                with conn.cursor() as c:
                    c.execute("SET ob_query_timeout=300000000")
                    c.execute("SET ob_trx_timeout=300000000")
                print("SQL ready", process.pid, flush=True)
                try:
                    (a.run / ("restart-maps.log" if restart else "process-maps.log")).write_text(
                        Path(f"/proc/{process.pid}/maps").read_text())
                except OSError:
                    pass
                return
            except pymysql.MySQLError:
                time.sleep(.5)
        raise TimeoutError("SQL startup exceeded 180s")

    def stop():
        nonlocal conn
        if conn:
            conn.close()
            conn = None
        if process is not None and process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=40)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=10)

    def query(c, sql, k):
        c.execute(sql)
        got = [int(row[0]) for row in c.fetchall()]
        if len(got) != k or len(set(got)) != k or not set(got) <= valid_ids:
            raise AssertionError(f"invalid IDs/cardinality: k={k}, got={got}")
        return got

    try:
        start()
        thread = threading.Thread(target=monitor, daemon=True)
        thread.start()
        with conn.cursor() as c:
            c.execute("ALTER SYSTEM SET ob_vector_memory_limit_percentage=30")
            c.execute("CREATE DATABASE amd_protocol")
            c.execute("USE amd_protocol")
            c.execute(f"CREATE TABLE items(id BIGINT PRIMARY KEY, v VECTOR({dim}))")
            c.execute("LOAD DATA LOCAL INFILE %s INTO TABLE items FIELDS TERMINATED BY '\\t' LINES TERMINATED BY '\\n' (id,v)",
                      (str((a.data / "base.tsv").resolve()),))
            c.execute("SELECT COUNT(*) FROM items")
            assert c.fetchone()[0] == len(ids)
            c.execute("SHOW INDEX FROM items")
            write_json(a.run / "indexes-before-ddl.json", list(c.fetchall()))
            result["phase"] = "building"
            write_json(a.run / "result.json", result)
            ddl = ("CREATE VECTOR INDEX idx_items ON items(v) WITH "
                   f"(distance=l2,type=hnsw,lib={a.lib},m=16,ef_construction=200,ef_search=64)")
            (a.run / "ddl.sql").write_text(ddl + ";\n")
            result["ddl_begin_monotonic_ns"] = time.monotonic_ns()
            result["ddl_begin_unix_ns"] = time.time_ns()
            t0 = time.perf_counter()
            c.execute(ddl)
            t_ddl = time.perf_counter()
            result["ddl_end_monotonic_ns"] = time.monotonic_ns()
            first = query(c, sqls[10][0], 10)
            t1 = time.perf_counter()
            result["first_query_end_monotonic_ns"] = time.monotonic_ns()
            result.update(build_seconds=t1-t0, ddl_seconds=t_ddl-t0, first_query_seconds=t1-t_ddl, first_ids=first)
            print("BUILD", result["build_seconds"], "DDL",result["ddl_seconds"], flush=True)
            write_json(a.run / "result.json", result)
            for k in (10, 100):
                predicted, latencies, hits = [], [], 0
                result[f"top{k}_begin_monotonic_ns"] = time.monotonic_ns()
                start_time = time.perf_counter()
                for i, sql in enumerate(sqls[k]):
                    before = time.perf_counter()
                    got = query(c, sql, k)
                    latencies.append(time.perf_counter() - before)
                    predicted.append(got)
                elapsed = time.perf_counter() - start_time
                result[f"top{k}_end_monotonic_ns"] = time.monotonic_ns()
                # Quality calculation and disk serialization are outside the QPS interval.
                for i, got in enumerate(predicted):
                    hits += len(set(got) & set(map(int, truth[i, :k])))
                np.savez(a.run / f"top{k}.npz", ids=np.asarray(predicted, dtype=np.int64), seconds=latencies)
                result[f"top{k}"] = {"seconds": elapsed, "qps": a.queries/elapsed,
                                      "recall": hits/(k*a.queries), "p95_seconds": float(np.percentile(latencies,95)),
                                      "valid_unique_ids": True}
                write_json(a.run / "result.json", result)
                print("TOP", k, result[f"top{k}"], flush=True)
        if a.restart:
            stop()
            start(restart=True)
            with conn.cursor() as c:
                c.execute("USE amd_protocol")
                for k in (10, 100):
                    got = query(c, sqls[k][0], k)
                    result[f"restart_top{k}"] = {"ids": got, "recall": len(set(got)&set(map(int,truth[0,:k])))/k}
            result["restart_sql_ok"] = True
        result["status"] = "PASS_DEVELOPMENT_EXECUTION"
    except Exception as exc:
        result["status"] = "FAIL"
        result["error"] = repr(exc)
        raise
    finally:
        stop_monitor.set()
        stop()
        (a.run / "resources.jsonl").write_text("".join(json.dumps(s)+"\n" for s in samples))
        result["vram_peak_bytes"] = max((v for s in samples for key,v in s.items()
                                         if key in {str(f) for f in gpu_files}), default=0)
        result["finished_unix"] = time.time()
        write_json(a.run / "result.json", result)
        print(json.dumps(result, indent=2), flush=True)


if __name__ == "__main__":
    main()
