# DPDK FFI Bindings Tools

這個目錄包含用於生成 DPDK FFI 綁定的腳本。

## 概述

tokio-dpdk 使用預生成的 DPDK FFI 綁定（checked into repo），避免在編譯時依賴 libclang。綁定文件位於：

```
tokio/src/runtime/scheduler/dpdk/ffi/
├── bindings_linux.rs    # Linux 預生成綁定
├── bindings_windows.rs  # Windows 預生成綁定
├── dpdk_wrappers.c      # C wrapper 用於 inline 函數
└── mod.rs               # 模組定義
```

## 何時需要重新生成

通常**不需要**重新生成綁定。只有在以下情況需要：

1. 升級 DPDK 版本（API 變更）
2. 需要使用新的 DPDK API

## Linux 綁定生成

### 前提條件

```bash
# 安裝 bindgen CLI
cargo install bindgen-cli

# 確保 DPDK 已安裝並可通過 pkg-config 找到
pkg-config --exists libdpdk && echo "DPDK found"
```

### 執行

```bash
./generate_dpdk_bindings.sh
```

輸出將被複製到 `tokio/src/runtime/scheduler/dpdk/ffi/bindings_linux.rs`。

### 自訂輸出路徑

```bash
TOKIO_FFI_DIR=/path/to/ffi ./generate_dpdk_bindings.sh
```

## Windows 綁定生成

### 前提條件

1. Visual Studio with C++ Build Tools
2. LLVM/Clang（用於 bindgen）
3. DPDK 源碼或已安裝的 headers
4. bindgen CLI：`cargo install bindgen-cli`

### 環境變數

| 變數 | 必需 | 說明 |
|------|------|------|
| `DPDK_DIR` | ✅ | DPDK 源碼或安裝路徑 |
| `LLVM_DIR` | 可選 | LLVM 安裝路徑（如果不在 PATH 中）|
| `VCVARS_PATH` | 可選 | vcvarsall.bat 路徑（自動檢測）|

### 執行

```cmd
set DPDK_DIR=C:\dpdk\dpdk-23.11
generate_dpdk_bindings_windows.bat
```

## 新增 DPDK API

如果需要使用新的 DPDK 函數：

1. 在腳本中添加 `--allowlist-function "rte_xxx_xxx"`
2. 重新生成綁定
3. 在 `mod.rs` 中添加對應的 safe wrapper（如需要）
4. 如果是 inline 函數，需要在 `dpdk_wrappers.c` 中添加 wrapper

## Inline 函數

DPDK 的 `rte_eth_rx_burst` 等核心函數是 inline 的，無法直接通過 FFI 調用。這些函數通過 C wrapper 暴露：

```c
// dpdk_wrappers.c
uint16_t dpdk_wrap_rte_eth_rx_burst(uint16_t port_id, uint16_t queue_id,
                                     struct rte_mbuf **rx_pkts, const uint16_t nb_pkts) {
    return rte_eth_rx_burst(port_id, queue_id, rx_pkts, nb_pkts);
}
```

Wrapper 函數在 `build.rs` 中編譯並鏈接。
