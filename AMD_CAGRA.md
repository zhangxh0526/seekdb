# AMD CAGRA 构建优化

基线为 V5 冻结 commit `634888d9b037153fa143d2c913ba4b25503e1d62`。实现保留 FP32 输入、100 轮 NN-descent、128/256 图度数和固定 hipVS CAGRA 搜索。

## 实现

`tools/hipvs_bridge/seekdb_cuvs_bridge.c` 在创建 GPU 资源和建图期间，将 LLVM OpenMP 的新线程默认值限制为最多 16 个，并保留调用线程已有的更低限制。CAGRA 内部新建线程，因此必须同时管理新线程默认值和当前线程设置。等待中的 OpenMP 工作线程立即休眠，避免在 GPU 执行时消耗 CPU 配额。临时配置由进程内互斥锁串行管理，构建成功或失败后均恢复调用线程及新线程默认的数量和等待设置。无法读取默认值时使用原 GPU 构建路径。该限制与输入维度、数据内容和评测阶段无关。

Linux GPU 构建为内部 pthread 保留至少 1 MiB 栈，容纳 GPU profiler 的静态 TLS。该规则同时适用于显式申请小栈的线程池；普通运行和 profiler 使用相同代码。

## 编译

需要组织方冻结的离线依赖、ROCm 7.2.4、hipVS `f96a663d0a4441d6f5344bd3ef552b71ae7b370b`、GCC 13.3、CMake 3.31 和 Rust 1.97。组织方提供 `/opt/hipvs`、`/opt/rocm` 及冻结的 `deps/3rd`、Rust vendor。无需联网下载新依赖。

```bash
HIPVS_ROOT=/opt/hipvs ROCM_ROOT=/opt/rocm \
  tools/hipvs_bridge/build.sh /workspace/amd-bridge/libseekdb_cuvs_bridge.so
cmake -S . -B build_release -G "Unix Makefiles" \
  -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DOB_ENABLE_UNITY=ON \
  -DOB_USE_LLD=ON \
  -DOB_BUILD_CUVS=ON \
  -DOB_BUILD_CUVS_TRACE=OFF \
  -DCUVS_BRIDGE_LIB=/workspace/amd-bridge/libseekdb_cuvs_bridge.so
cmake --build build_release --target seekdb
```

可通过 `CMAKE_BUILD_PARALLEL_LEVEL` 环境变量控制编译并行度，不改变上述编译选项。产物为 `build_release/src/observer/seekdb` 和指定的 bridge 动态库。

## 开发验证

[测试协议](docs/gpu-vector-index-hipvs-cuvs/contest/BENCHMARK_PROTOCOL.md)定义数据、计时边界、单行 SQL、质量、资源和配对规则。开发驱动需要 NumPy、PyMySQL；读取 SIFT HDF5 时另需 h5py。云端预装 Python 环境已提供这些依赖，它们不是新增的 seekdb 运行时依赖。

```bash
python tools/hipvs_bridge/benchmark/prepare_data.py \
  --sift-hdf5 /path/to/sift-128-euclidean.hdf5 \
  --rows 250000 --queries 4096 --out /workspace/amd-data/sift250k
python tools/hipvs_bridge/benchmark/sql_benchmark.py \
  --binary "$PWD/build_release/src/observer/seekdb" \
  --bridge /workspace/amd-bridge/libseekdb_cuvs_bridge.so \
  --data /workspace/amd-data/sift250k \
  --run /workspace/amd-runs/candidate-sift --restart
```

每次 `--run` 必须是尚不存在的目录。`--profile --queries 16` 用于独立 GPU 路径采样；`--lib vsag` 用于 CPU SQL 对比。正式查询测试保持默认每组 4,096 条单行 SQL。具体数据散列、二进制/bridge 散列、原始查询结果、资源采样及阶段时间写入运行目录。

线程配置恢复检查会真实并发构建两个 GPU 索引，验证不同调用线程的限制及新线程默认值均能恢复：

```bash
python tools/hipvs_bridge/benchmark/prepare_data.py \
  --rows 10000 --queries 100 --dim 128 --out /workspace/amd-data/scope
python tools/hipvs_bridge/benchmark/check_omp_scope.py \
  --bridge /workspace/amd-bridge/libseekdb_cuvs_bridge.so \
  --data /workspace/amd-data/scope
```

公开子集和固定种子的 256 维代理仅用于开发；正式 Hidden250K 门槛和成绩以平台结果为准。
