# DPDK 測試重構任務

## 目標
- 使用 `rusty-fork` 進行子進程隔離
- 統一從 `env.json` 讀取配置
- 端口從環境變量 `DPDK_TEST_PORT` 讀取（無默認值）

## 需要修改的文件

### 已完成
- [x] `tokio/Cargo.toml` - 添加 rusty-fork 依賴
- [x] `tokio/tests/support/dpdk_config.rs` - 創建共享配置模組
- [x] `tokio/tests/rt_common.rs` - 更新 dpdk_scheduler 從 env.json 讀取
- [x] `tokio/tests/tcp_dpdk.rs` - 移除 DPDK_DEVICE 環境變量，使用 env.json
- [x] `tokio/tests/tcp_dpdk_real.rs` - 移除 DPDK_DEVICE 環境變量，使用 env.json  
- [x] `tokio/tests/dpdk_worker_isolation.rs` - 使用 env.json 和 DPDK_TEST_PORT
- [x] `tokio/tests/dpdk_multi_process.rs` - 使用 env.json
- [x] `tokio/tests/time_sleep.rs` - 更新註釋
- [x] `tokio-macros/src/entry.rs` - 更新 DPDK flavor 從 env.json 讀取
- [x] `run_dpdk_tests.sh` - 移除 DPDK_DEVICE，添加 DPDK_TEST_PORT

## 配置來源
- DPDK 設備 PCI 地址：從 `/etc/dpdk/env.json` 讀取 role="dpdk" 的設備
- Kernel IP 地址：從 env.json 讀取 role="kernel" 且 IPv4 地址最多的設備
- 測試端口：從 `DPDK_TEST_PORT` 環境變量讀取（必須設置）

## 使用方式
```bash
DPDK_TEST_PORT=8192 ./run_dpdk_tests.sh
```
