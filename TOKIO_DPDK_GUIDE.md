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
- **Root 權限** — 必須以 `sudo` 執行（見下方平台說明）

### 平台特定：AWS EC2（非裸機）

EC2 虛擬機（非 bare-metal 實例）**不支援 IOVA VA（虛擬地址）模式**，因為 guest OS 無法存取真實的 IOMMU。DPDK 在此環境下必須使用 **IOVA PA（物理地址）模式**，這帶來一個硬性限制：

> **所有 DPDK 相關操作必須以 root 執行。** PA 模式需要讀取 `/proc/self/pagemap` 來取得物理地址映射，這是一個特權操作。

**影響範圍：**

| 場景 | 做法 |
|------|------|
| **應用程式** | `sudo ./your_app` 或 `sudo cargo-fast run` |
| **測試** | 設定 `SERIAL_ISOLATION_SUDO=1`，隔離測試宏會自動對子進程使用 `sudo` |
| **建置** | `cargo-fast build` 本身不需要 root，只有**執行**時需要 |

**EAL 會自動偵測並使用 PA 模式**，不需要手動傳 `--iova-mode=pa`（除非你有特殊需求）。

> **裸機實例**（如 `m5.metal`、`c5.metal`）支援 IOMMU 和 VA 模式，可能不需要 root。但為統一性建議仍以 root 執行。

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
    devices: Vec<DeviceConfig>,      // 裝置列表
    dpdk_cores: Vec<usize>,          // 可用核心列表（必填）
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
    original_name: Option<String>,   // 原始介面名稱
}
```

> **注意**：配置文件中的 `version`、`platform`、`generated_at` 字段會被忽略（用於人類閱讀）。`eal_args` 字段會被透傳至 `rte_eal_init()`，與 Builder API 的 `dpdk_eal_args()` 合併（env.json 優先，Builder 可覆蓋）。

### smoltcp 環境變量（必須！）

依賴 tokio-dpdk 的項目**必須**在項目根目錄創建 `.cargo/config.toml`，設置 smoltcp 的 IP 地址限制：

```toml
[env]
# 允許 smoltcp 接口最多配置 128 個 IP 地址
# 這是 AWS ENI 多 IP 配置所必需的
SMOLTCP_IFACE_MAX_ADDR_COUNT = "128"

[build]
rustflags = ["-C", "target-cpu=native"]
```

> **警告**：如果缺少此配置，smoltcp 將使用默認限制（約 8 個 IP），導致多餘的 IP 地址添加失敗並產生大量警告：
> ```
> Warning: Failed to add IP address 172.31.1.42/24: ...
> ```

**JSON 範例**（完整範例見 `scripts/dpdk/templates/env.example.json`）：

```json
{
  "dpdk_cores": [1, 2, 3, 4],
  "devices": [
    {
      "pci_address": "0000:28:00.0",
      "mac": "02:04:3d:ba:a3:2f",
      "addresses": ["172.31.1.40/20", "172.31.1.41/20", "2406:da18:e99:5d00::10/64"],
      "gateway_v4": "172.31.0.1",
      "gateway_v6": "2406:da18:e99:5d00::1",
      "mtu": 9001,
      "role": "dpdk",
      "original_name": "enp40s0"
    }
  ]
}
```

> **重要**：`dpdk_cores` 是必填字段，指定 DPDK 運行時可用的 CPU 核心。建議保留 CPU 0 給 kernel。

---

## 配置

### 配置流程概述

tokio-dpdk runtime 依賴一個配置文件來獲取裝置的網路配置（IP、MAC、Gateway）。典型流程：

```
1. 執行 setup.sh wizard    →  生成 /etc/dpdk/env.json（包含 NIC、IP、MAC、核心等所有配置）
2. 在代碼中建立 runtime    →  Builder::new_dpdk().enable_all().build()
3. runtime 啟動            →  自動從 env.json 讀取配置，初始化 DPDK 和 smoltcp
```

### 配置文件

Runtime 在以下位置搜索配置文件（按順序）：

1. `DPDK_ENV_CONFIG` 環境變數指定的路徑
2. `./config/dpdk-env.json`（項目本地）
3. `/etc/dpdk/env.json`（系統級，由 `setup.sh` 生成）

配置文件由 `scripts/dpdk/setup.sh dpdk-bind` 自動生成。

### 建立 DPDK Runtime

**前提條件**：確保已執行 `setup.sh` 生成配置文件（`/etc/dpdk/env.json`）。

Runtime 會**自動**從 `env.json` 讀取所有 NIC 裝置、IP、MAC、Gateway、核心分配。你的代碼中**不需要處理任何硬體細節** — 只需要決定要幾個 worker：

```rust
use tokio::runtime::Builder;

// 使用所有可用裝置和核心
let rt = Builder::new_dpdk()
    .enable_all()
    .build()
    .expect("DPDK runtime creation failed");

// 指定 worker 數量
let rt = Builder::new_dpdk()
    .worker_threads(1)
    .enable_all()
    .build()
    .expect("DPDK runtime creation failed");
```

**就這樣。** 不需要指定 PCI 地址、IP、MAC 或任何硬體配置。`env.json` 已經包含一切，runtime 會自動處理裝置發現和資源分配。

### 多進程資源隔離

當多個 DPDK 進程在同一機器上運行時，`tokio-dpdk` 使用檔案鎖機制（`/var/run/dpdk/*.lock`）確保資源互斥：

- **裝置鎖定**：每個進程獲取 NIC PCI 地址的排他鎖
- **核心鎖定**：從 `dpdk_cores` 列表中自動選取未被鎖定的核心
- **自動釋放**：進程崩潰時 OS 自動釋放鎖，新進程可立即接管

多進程場景下也不需要手動指定 PCI 地址。資源鎖機制會讓每個進程自動搶佔未使用的裝置和核心。

### Builder 方法

| 方法 | 說明 |
|------|------|
| `new_dpdk()` | 建立 DPDK runtime builder |
| `worker_threads(usize)` | 指定 worker 數量（預設使用 `env.json` 中所有 `dpdk_cores`） |
| `dpdk_eal_arg(&str)` | 添加單個 EAL 參數 |
| `dpdk_eal_args(&[&str])` | 添加多個 EAL 參數 |
| `dpdk_mempool_size(u32)` | 設定 DPDK 記憶體池大小（mbuf 數量，預設 8192） |
| `dpdk_cache_size(u32)` | 設定每核心記憶體池快取大小（預設 256） |
| `dpdk_queue_descriptors(u16)` | 設定每個隊列的 RX/TX 描述符數量（預設 128） |
| `enable_all()` | 啟用所有功能（I/O、time） |
| `dpdk_pci_addresses(&[&str])` | **極少使用。** 過濾到特定 PCI 裝置。省略時自動使用 env.json 中全部 DPDK 裝置 |
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
| `DPDK_DEBUG` | 啟用流量規則等調試輸出 |

---

## API 參考

### TCP 網路

tokio-dpdk 提供兩種獨立的 TCP 實作：

| API | 網路路徑 | 說明 |
|-----|----------|------|
| `tokio::net::TcpStream` | 內核網路 | 標準 mio/epoll，適用於一般用途 |
| `tokio::net::TcpDpdkStream` | DPDK 用戶態 | 繞過內核，超低延遲 |

**重要**：這兩個是**完全獨立的類型**。在 DPDK runtime 中使用 `TcpStream` 仍然會走內核網路。

### 使用 DPDK 網路

```rust
use tokio::net::TcpDpdkStream;

// 連接到遠程服務器（使用 DPDK 用戶態網路）
let mut stream = TcpDpdkStream::connect("10.0.0.1:8080").await?;

// 使用 AsyncRead/AsyncWrite trait
stream.write_all(b"hello").await?;
let mut buf = [0u8; 1024];
let n = stream.read(&mut buf).await?;
```

### 使用內核網路

即使在 DPDK runtime 中，你仍然可以使用標準 `TcpStream` 走內核網路：

```rust
use tokio::net::TcpStream;

// 這會使用內核網路，不是 DPDK
let stream = TcpStream::connect("127.0.0.1:8080").await?;
```

### DPDK Stream 特有方法

```rust
use tokio::net::TcpDpdkStream;

let stream = TcpDpdkStream::connect("10.0.0.1:8080").await?;

// 取得此 stream 綁定的 CPU 核心 ID
let core_id = stream.core_id();

// 檢查連接是否有效
if stream.is_connected() {
    // ...
}
```

### TcpDpdkSocket - 連接前配置

`TcpDpdkSocket` 允許在建立連接前配置 socket 選項：

```rust
use tokio::net::TcpDpdkSocket;
use std::net::SocketAddr;

// 創建 socket 並配置選項
let socket = TcpDpdkSocket::new_v4()?;
socket.set_nodelay(true)?;
socket.set_send_buffer_size(128 * 1024)?;
socket.set_recv_buffer_size(128 * 1024)?;

// 可選：綁定到特定本地地址
let local_addr: SocketAddr = "172.31.1.40:12345".parse()?;
socket.bind(local_addr)?;

// 連接
let stream = socket.connect("10.0.0.1:8080".parse()?).await?;

// 驗證本地地址
assert_eq!(stream.local_addr()?.port(), 12345);
```

### TcpDpdkListener - 服務端監聽

```rust
use tokio::net::TcpDpdkListener;

// 綁定到 DPDK 接口的 IP（必須是 env.json 中配置的 IP）
let listener = TcpDpdkListener::bind("172.31.1.40:8080").await?;

// 設置 TTL
listener.set_ttl(64)?;

loop {
    let (stream, peer_addr) = listener.accept().await?;
    // 處理連接...
}
```

### API 對比表

| 方法 | TcpStream (kernel) | TcpDpdkStream | 說明 |
|------|-------------------|---------------|------|
| `connect()` | ✅ | ✅ | 支援 ToSocketAddrs (DNS 解析) |
| `read()`/`write()` | ✅ | ✅ | AsyncRead/AsyncWrite trait |
| `readable()`/`writable()` | ✅ | ✅ | 等待就緒狀態 |
| `try_read()`/`try_write()` | ✅ | ✅ | 非阻塞讀寫 |
| `peek()` | ✅ | ✅ | 讀取但不消費數據 |
| `set_nodelay()`/`nodelay()` | ✅ | ✅ | 禁用 Nagle 算法 |
| `set_ttl()`/`ttl()` | ✅ | ✅ | IP 封包 TTL |
| `local_addr()`/`peer_addr()` | ✅ | ✅ | 地址查詢 |
| `split()`/`into_split()` | ✅ | ✅ | 分割讀寫半 |
| `shutdown()` | ✅ | ✅ | 關閉寫端 |
| `core_id()` | ❌ | ✅ | **DPDK 專用：worker 核心 ID** |
| `from_std()`/`into_std()` | ✅ | ❌ | 無用戶態 socket 概念 |
| `linger()`/`set_linger()` | ✅ | ❌ | smoltcp 不支援 |

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


## 測試

### 執行 DPDK 測試

DPDK 測試使用 `#[serial_isolation_test]` 宏實現進程隔離，確保每個測試在獨立進程中執行，避免 EAL 重複初始化問題。

**1. 設置環境變數**

在 `.env` 文件中配置（此文件不應提交到 git）：
```bash
# .env
DPDK_TEST_PORT=8192
```

**2. 運行所有測試**

```bash
source .env && export DPDK_TEST_PORT
SERIAL_ISOLATION_SUDO=1 cargo test --features "full,test-util"
```

**3. 運行特定測試**

```bash
source .env && export DPDK_TEST_PORT
SERIAL_ISOLATION_SUDO=1 cargo test --manifest-path tokio/Cargo.toml \
    --test tcp_dpdk --features full -- api_parity::tcp_stream_read_write
```

### 編寫 DPDK 測試

> **`#[tokio::test(flavor = "dpdk")]` 和 `#[tokio::main(flavor = "dpdk")]` 不受支援。**
> DPDK runtime 的配置需求（PCI 地址、worker 數量、EAL 參數、子進程隔離）不適合用屬性宏表達。
> 必須使用 `Builder::new_dpdk()` 手動建立 runtime。

**標準模式：手動建立 runtime + 子進程隔離**

```rust
#[serial_isolation_test::serial_isolation_test]
#[test]
fn my_dpdk_test() {
    let rt = tokio::runtime::Builder::new_dpdk()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("DPDK runtime creation failed");

    rt.block_on(async {
        let stream = TcpDpdkStream::connect("1.1.1.1:80").await.unwrap();
        // ...
    });
}
```

**要點**：
- 所有 DPDK 測試**必須**使用 `#[serial_isolation_test]` 確保子進程隔離（EAL 每進程只能初始化一次）
- `SERIAL_ISOLATION_SUDO=1` 環境變數會讓隔離宏自動以 `sudo` 執行子進程（EC2 PA 模式要求 root）
- 不要使用 `#[should_panic]` — 使用 `std::panic::catch_unwind` 代替
- 不要使用 `#[tokio::test]` 或 `#[tokio::main]` 搭配 `flavor = "dpdk"` — 此組合不受支援

### Panic 測試模式

由於 `#[should_panic]` 與子進程隔離不兼容，使用以下模式：

```rust
#[serial_isolation_test::serial_isolation_test]
#[test]
fn test_expected_panic() {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 預期會 panic 的代碼
    }));
    assert!(result.is_err(), "Expected panic but test passed");
}
```

### 測試類別

| 類別 | 說明 |
|------|------|
| `dpdk_rt_common::*` | 核心 runtime 相容性測試 (45 個) |
| `tcp_dpdk::*` | TCP/網路 API 相容性測試 |
| `tcp_dpdk_real::*` | 真實網路連接測試 |
| `time_sleep::dpdk_flavor::*` | 計時器功能測試 |
| `cpu_affinity_tests::*` | CPU 核心綁定驗證 |
| `dpdk_multi_process::*` | 多進程資源鎖定驗證 |

### 環境變數

| 變數 | 說明 |
|------|------|
| `DPDK_TEST_PORT` | 測試用監聽端口（必須設置，建議 8192） |
| `SERIAL_ISOLATION_SUDO` | 設為 `1` 時，`#[serial_isolation_test]` 宏會自動對子進程使用 `sudo`。**僅用於測試**；正常應用程式應直接以 `sudo` 執行 |
| `DPDK_DEVICE` | 覆蓋預設 PCI 位址（可選） |

### 執行 DPDK 應用程式

DPDK 應用程式**必須以 root 執行**（EC2 非裸機需要 PA 模式讀取 `/proc/self/pagemap`）：

```bash
# 直接執行
sudo ./target/release/my_dpdk_app

# 透過 cargo 執行
sudo cargo-fast run --release -p my_dpdk_app
```

> `SERIAL_ISOLATION_SUDO` 只影響測試宏的子進程行為，**不影響**正常應用程式。正常應用程式需要手動以 `sudo` 啟動。

---

## 已知限制

### 1. EAL 單例

DPDK 的 EAL **每個進程只能初始化一次**。這意味著：
- 無法依序建立多個 DPDK runtime
- 嵌套 runtime 建立會失敗
- 測試必須在獨立進程中執行（由 `#[serial_isolation_test]` 宏自動處理）

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

1. 運行 `SERIAL_ISOLATION_SUDO=1 cargo test --features "full,test-util"` 驗證所有測試通過
2. 在真實硬體上測試
3. 記錄任何新的架構限制
4. 為 API 變更更新此指南

---

*最後更新：2026-01-28*
