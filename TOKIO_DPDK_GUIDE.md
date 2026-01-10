# Tokio-DPDK 整合指南

本文件提供 DPDK 整合到此 Tokio fork 的完整指南，涵蓋架構、配置、API 使用及已知限制。

## 目錄

1. [概述](#概述)
2. [先決條件](#先決條件)
3. [配置](#配置)
4. [API 參考](#api-參考)
5. [架構](#架構)
6. [測試](#測試)
7. [已知限制](#已知限制)
8. [疑難排解](#疑難排解)

---

## 概述

此 Tokio fork 新增了針對超低延遲網路的 **DPDK 調度器**。主要特性：

- **Busy-poll 事件迴圈** - Worker 永不休眠，持續輪詢網路事件
- **每核心 CPU 親和性** - 每個 worker 綁定到特定 CPU 核心
- **smoltcp TCP/IP 協議棧** - 基於 DPDK 的用戶空間網路
- **EAL 單例** - DPDK 的環境抽象層 (EAL) 每個進程只能初始化一次

### 何時使用 DPDK Runtime

適合使用 DPDK runtime 的場景：
- 需要亞微秒級網路延遲
- 高頻交易或即時系統需要內核繞過
- 需要專用 CPU 核心處理網路 I/O

一般用途的異步 I/O 請使用標準 Tokio runtime。

---

## 先決條件

### 系統要求

- **Linux** (kernel 4.14+)
- **DPDK 23.11+** 系統級安裝
- **Hugepages** 已配置 (2MB 或 1GB)
- **VFIO-PCI** 或 **igb_uio** 驅動程式已綁定到網路介面
- **Root 權限** (或 CAP_NET_ADMIN + CAP_SYS_ADMIN)

### 設定腳本

本倉庫包含完整的 DPDK 環境設定工具，位於 `scripts/dpdk/`。

執行統一設定精靈：

```bash
cd /path/to/tokio-dpdk
sudo ./scripts/dpdk/setup.sh wizard    # 互動式一站設定
sudo ./scripts/dpdk/setup.sh verify    # 驗證配置
sudo ./scripts/dpdk/setup.sh detect    # 檢測硬件
```

詳細說明請參閱 [`scripts/dpdk/README.md`](scripts/dpdk/README.md)。

### 環境配置文件格式

DPDK 裝置透過 `/etc/dpdk/env.json` 配置（由 `setup.sh` 自動生成）。

配置結構定義於 [`tokio/src/runtime/scheduler/dpdk/env_config.rs`](tokio/src/runtime/scheduler/dpdk/env_config.rs)：

```rust
// 頂層配置
struct DpdkEnvConfig {
    version: u32,                    // Schema 版本 (當前為 1)
    generated_at: Option<String>,    // 生成時間
    platform: String,                // 平台標識 (e.g., "aws-ec2")
    devices: Vec<DeviceConfig>,      // 裝置列表
    eal_args: Vec<String>,           // 額外 EAL 參數
}

// 單一裝置配置
struct DeviceConfig {
    pci_address: String,             // PCI 位址 (e.g., "0000:28:00.0")
    mac: [u8; 6],                    // MAC 地址
    addresses: Vec<IpCidr>,          // IP 位址列表 (支援 IPv4/IPv6)
    gateway_v4: Option<Ipv4Address>, // IPv4 閘道
    gateway_v6: Option<Ipv6Address>, // IPv6 閘道
    mtu: u16,                        // MTU (AWS ENA 支援 9001)
    role: DeviceRole,                // "dpdk" 或 "kernel"
    core_affinity: Option<usize>,    // 綁定的 CPU 核心
    original_name: Option<String>,   // 原始介面名稱
}
```

**JSON 範例**（完整範例見 `scripts/dpdk/templates/env.example.json`）：

```json
{
  "version": 1,
  "platform": "aws-ec2",
  "devices": [
    {
      "pci_address": "0000:28:00.0",
      "mac": "02:04:3d:ba:a3:2f",
      "addresses": ["172.31.1.40/20", "2406:da18:e99:5d00::10/64"],
      "gateway_v4": "172.31.0.1",
      "gateway_v6": "2406:da18:e99:5d00::1",
      "mtu": 9001,
      "role": "dpdk",
      "core_affinity": 1,
      "original_name": "enp40s0"
    }
  ],
  "eal_args": ["--iova-mode=pa"]
}
```

---

## 配置

### 配置流程概述

tokio-dpdk runtime 依賴一個配置文件來獲取裝置的網路配置（IP、MAC、Gateway）。典型流程：

```
1. 執行 setup.sh wizard    →  生成 /etc/dpdk/env.json
2. 在代碼中指定裝置 PCI    →  runtime 從 env.json 讀取配置
3. runtime 啟動            →  使用配置初始化 DPDK 和 smoltcp
```

### 配置文件

Runtime 在以下位置搜索配置文件（按順序）：

1. `DPDK_ENV_CONFIG` 環境變數指定的路徑
2. `./config/dpdk-env.json`（項目本地）
3. `/etc/dpdk/env.json`（系統級，由 `setup.sh` 生成）

配置文件由 `scripts/dpdk/setup.sh dpdk-bind` 自動生成。

### 建立 DPDK Runtime

**前提條件**：確保已執行 `setup.sh` 生成配置文件。

```rust
use tokio::runtime::Builder;

// 基本 DPDK runtime - 指定裝置的 PCI 位址
// IP、MAC、Gateway 會從 /etc/dpdk/env.json 自動讀取
let rt = Builder::new_dpdk()
    .dpdk_device("0000:28:00.0")  // PCI 位址
    .enable_all()
    .build()
    .expect("DPDK runtime creation failed");

// 多裝置 runtime - 每個裝置一個 worker
let rt = Builder::new_dpdk()
    .dpdk_devices(&["0000:28:00.0", "0000:29:00.0"])
    .enable_all()
    .build()
    .expect("Multi-device DPDK runtime creation failed");

// 自訂 EAL 參數
let rt = Builder::new_dpdk()
    .dpdk_device("0000:28:00.0")
    .dpdk_eal_args(&["--iova-mode=pa", "--no-telemetry"])
    .enable_all()
    .build()
    .expect("DPDK runtime creation failed");
```

### Builder 方法

| 方法 | 說明 |
|------|------|
| `new_dpdk()` | 建立新的 DPDK runtime builder |
| `dpdk_device(&str)` | 指定單一裝置（PCI 位址或原始介面名） |
| `dpdk_devices(&[&str])` | 指定多個裝置（每個裝置一個 worker） |
| `dpdk_eal_arg(&str)` | 添加單個 EAL 參數 |
| `dpdk_eal_args(&[&str])` | 添加多個 EAL 參數 |
| `enable_all()` | 啟用所有功能（I/O、time 等） |
| `enable_io()` | 僅啟用 I/O 驅動程式 |
| `enable_time()` | 僅啟用計時器驅動程式 |

### 環境變數

| 變數 | 說明 |
|------|------|
| `DPDK_ENV_CONFIG` | 覆蓋配置文件路徑 |
| `DPDK_DEVICE` | 測試用：指定預設 PCI 位址 |

---

## API 參考

### TCP 網路

DPDK runtime 提供標準 Tokio 網路類型的替代：

```rust
use tokio::net::{TcpListener, TcpStream};

// 使用方式與標準 Tokio 相同
let listener = TcpListener::bind("10.0.0.1:8080").await?;
let (stream, addr) = listener.accept().await?;
```

**注意：** 使用 DPDK runtime 時，`TcpListener` 和 `TcpStream` 會自動在內部使用 DPDK 實作。

### DPDK 專用 API

```rust
use tokio::runtime::dpdk::TcpDpdkStream;

// 取得此 stream 綁定的 CPU 核心 ID
let core_id = stream.core_id();

// 檢查連接是否有效
if stream.is_connected() {
    // ...
}
```

### 阻塞操作

阻塞操作正常運作，但在獨立的（非 DPDK）執行緒上執行：

```rust
let result = tokio::task::spawn_blocking(|| {
    // 這在阻塞執行緒上執行，不是在 DPDK worker 核心上
    std::fs::read_to_string("config.toml")
}).await?;
```

---

## 架構

### 調度器結構

```
┌─────────────────────────────────────────────────────────────┐
│                    DPDK Runtime                             │
├─────────────────────────────────────────────────────────────┤
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │   Worker 0  │  │   Worker 1  │  │   I/O 執行緒        │ │
│  │  (核心 0)   │  │  (核心 1)   │  │  (標準 I/O)         │ │
│  │             │  │             │  │                     │ │
│  │ DpdkDriver  │  │ DpdkDriver  │  │  計時器驅動程式     │ │
│  │ smoltcp     │  │ smoltcp     │  │  信號處理           │ │
│  │ 任務佇列    │  │ 任務佇列    │  │                     │ │
│  └─────────────┘  └─────────────┘  └─────────────────────┘ │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐   │
│  │              阻塞執行緒池                            │   │
│  │  (標準 Tokio blocking，無 CPU 親和性)               │   │
│  └─────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

### 關鍵組件

1. **DpdkDriver** - 每個 worker 的 DPDK 封包 I/O
2. **smoltcp** - 用戶空間 TCP/IP 協議棧
3. **I/O 執行緒** - 處理計時器和標準 I/O (非 DPDK)
4. **Inject Queue** - 跨執行緒任務調度

### Worker 親和性

每個 DPDK worker 綁定到特定 CPU 核心。從某個 worker 產生的任務會在該 worker 的核心上執行。

**重要：** `TcpDpdkStream` 有 worker 親和性 - 必須在建立它的同一個 worker 上使用。

---

## 測試

### 執行 DPDK 測試

```bash
# 執行所有 DPDK 測試（需要 root 和 DPDK 環境）
sudo -i bash -c 'source ~/.cargo/env && cd /path/to/tokio-dpdk && ./run_dpdk_tests.sh'

# 執行特定測試
sudo -i bash -c 'source ~/.cargo/env && cd /path/to/tokio-dpdk && \
    DPDK_DEVICE="0000:28:00.0" cargo test --package tokio --test tcp_dpdk \
    --features full -- api_parity::tcp_stream_read_write --exact --nocapture'
```

### 測試類別

| 類別 | 說明 |
|------|------|
| `rt_common::dpdk_scheduler::*` | 核心 runtime 相容性測試 |
| `tcp_dpdk::*` | TCP/網路 API 相容性測試 |
| `tcp_dpdk_real::*` | 真實網路連接測試 |
| `time_sleep::dpdk_flavor::*` | 計時器功能測試 |
| `cpu_affinity_tests::*` | CPU 核心綁定驗證 |

### 跳過的測試

以下測試因 DPDK 架構限制而被刻意跳過：

**EAL 重複初始化：**
- `create_rt_in_block_on` - 嵌套 runtime 建立
- `runtime_in_thread_local` - thread_local 中的多個 runtime
- `shutdown_concurrent_spawn` - 迴圈建立多個 runtime
- `io_notify_while_shutting_down` - 迴圈建立多個 runtime

**Park 語義：**
- `yield_defers_until_park` - DPDK 使用 busy-poll（無 park 階段）
- `coop_yield_defers_until_park` - DPDK 使用 busy-poll（無 park 階段）

---

## 已知限制

### 1. EAL 單例

DPDK 的 EAL **每個進程只能初始化一次**。這意味著：
- 無法依序建立多個 DPDK runtime
- 嵌套 runtime 建立會失敗
- 測試必須在獨立進程中執行（由 `run_dpdk_tests.sh` 處理）

### 2. Busy-Poll 語義

DPDK worker 永不 park - 它們持續輪詢網路事件。這影響：
- `yield_now()` 行為不同（可能立即重新調度）
- CPU 使用率較高（設計如此）
- 無閒置睡眠

### 3. Worker 親和性

`TcpDpdkStream` 綁定到建立它的 worker（參考 `stream.rs:L200-210`）：

| 操作 | 錯誤 worker 上的行為 |
|------|---------------------|
| **讀/寫 (`poll_read`/`poll_write`)** | ⚠️ **panic!** |
| **Drop** | 記錄警告（不 panic，避免破壞 Drop 語義） |

**設計原因**：DPDK socket 狀態只存在於特定 worker 的 `DpdkDriver` 中，跨 worker 訪問會導致數據競爭。

### 4. 混雜模式

在某些 NIC（例如 AWS ENA）上，可能無法啟用混雜模式。這是非致命的：
```
Warning: Failed to enable promiscuous mode for port 0
```

---

## 疑難排解

### 常見問題

**1. "rte_eal_init failed"**
- 確保已配置 hugepages：`cat /proc/meminfo | grep Huge`
- 檢查 DPDK 驅動程式已綁定：`dpdk-devbind.py --status`
- 以 root 權限執行

**2. "DPDK_DEVICE environment variable not set"**
- 設定環境變數：`export DPDK_DEVICE=0000:28:00.0`
- 或配置 `/etc/dpdk/env.json`

**3. "Cannot drop runtime in async context"**
- 不要從異步任務內部 drop runtime
- 使用 `shutdown_timeout()` 進行優雅關閉

**4. 測試卡住**
- 部分測試有 60 秒超時
- 檢查 EAL 初始化問題（單獨執行測試）

### 調試日誌

啟用 DPDK 調試輸出：
```bash
RUST_LOG=tokio::runtime::scheduler::dpdk=debug cargo test ...
```

---

## 版本資訊

- **Tokio 基礎版本：** 1.49.0
- **DPDK 版本：** 23.11+
- **smoltcp 版本：** 0.11.0

---

## 貢獻

修改 DPDK 相關程式碼時：

1. 執行 `./run_dpdk_tests.sh` 驗證所有測試通過
2. 盡可能在真實硬體上測試
3. 記錄任何新的架構限制
4. 為 API 變更更新此指南

---

*最後更新：2026-01-10*
