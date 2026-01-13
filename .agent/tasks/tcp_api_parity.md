# TCP API 對等性實現與測試修復任務

## 背景與目的

### 目的
1. 為 `TcpDpdkStream`、`TcpDpdkListener` 補齊缺失的標準 tokio TCP API
2. 新增 `TcpDpdkSocket` 類型支援連接前配置
3. **修復所有現有測試，使其使用真正的 TcpDpdkStream 和 TcpDpdkListener**

### 嚴重問題：現有測試未使用 DPDK 網路棧

**經過代碼審查確認**：所有現有測試都使用 `tokio::net::TcpStream`（標準 kernel 網路），而非 `tokio::net::TcpDpdkStream`（smoltcp 用戶態網路）。

證據：
```rust
// tcp_dpdk.rs:L58
use tokio::net::{TcpListener, TcpStream};  // 標準類型！

// tcp_dpdk.rs:L77
let client = TcpStream::connect(addr).await.unwrap();  // 走 kernel！

// dpdk_worker_isolation.rs:L27
use tokio::net::{TcpListener, TcpStream};  // 標準類型！

// dpdk_worker_isolation.rs:L444
TcpStream::connect(server_addr),  // 走 kernel！
```

**這意味著 `TcpDpdkStream` 的核心功能從未被真實測試過！**

### 項目上下文

`tokio-dpdk` 提供基於 DPDK 和 smoltcp 的用戶態 TCP 實現。當前測試只驗證了「DPDK runtime 可以調度任務」，未驗證「DPDK 網路棧正確工作」。

### 現有實現狀態（基於代碼審查）

**TcpDpdkStream 已實現**（`tcp_dpdk/stream.rs`）：
- `connect(SocketAddr)` — 只接受 SocketAddr，無 DNS
- `local_addr()` / `peer_addr()`
- `core_id()` — DPDK 專用
- `set_nodelay()` / `nodelay()` — **有方法但是 no-op，需修復**
- `split()` / `into_split()`
- `poll_read_ready()` / `poll_write_ready()`
- `AsyncRead::poll_read`
- `AsyncWrite::poll_write` / `poll_flush` / `poll_shutdown`

**TcpDpdkListener 已實現**（`tcp_dpdk/listener.rs`）：
- `bind(SocketAddr)` — 只接受 SocketAddr，無 DNS
- `accept()` / `poll_accept()`
- `local_addr()` / `core_id()`

### 相關文件

**現有測試文件（需修復）**：

| 文件 | 問題行號 | 說明 |
|------|----------|------|
| `tcp_dpdk.rs` | L58, L240, L436, L566, L779, L844, L992, L1393, L1424, L1455 | 多處 import 標準 TcpStream |
| `tcp_dpdk_real.rs` | L23 | 使用標準 TcpStream 連接 Cloudflare |
| `dpdk_multi_process.rs` | L315, L344, L378, L408, L451, L490 | 多處使用標準 TcpStream |
| `dpdk_worker_isolation.rs` | L28, L444 | 使用標準 TcpStream |

**不需要修改的測試文件**（測試標準 kernel TCP，非 DPDK 相關）：
- `tcp_stream.rs`, `tcp_connect.rs`, `tcp_accept.rs`, `tcp_echo.rs`
- `tcp_shutdown.rs`, `tcp_split.rs`, `tcp_into_split.rs`, `tcp_into_std.rs`
- `tcp_peek.rs`, `tcp_socket.rs`, `rt_common.rs`, `rt_threaded.rs`

**現有實現**：
- `tokio/src/net/tcp_dpdk/stream.rs:L49-445`
- `tokio/src/net/tcp_dpdk/listener.rs:L47-240`
- `tokio/src/net/tcp_dpdk/split.rs`

**標準參考**：
- `tokio/src/net/tcp/stream.rs` — 標準 TcpStream
- `tokio/src/net/tcp/socket.rs` — 標準 TcpSocket


---

## 🚨 強制要求：測試必須使用真實 DPDK 網路棧

### 絕對禁止

1. **禁止使用 `tokio::net::TcpStream`** — 必須使用 `tokio::net::TcpDpdkStream`
2. **禁止使用 `tokio::net::TcpListener`** — 必須使用 `tokio::net::TcpDpdkListener`
3. **禁止任何 fallback 機制** — 測試不能在 DPDK 不可用時回退到 kernel 網路
4. **禁止模擬網路** — 必須使用真實的網際網路連接

### 必須滿足

1. 所有 TCP 客戶端測試使用 `TcpDpdkStream::connect`
2. 所有 TCP 服務端測試使用 `TcpDpdkListener::bind` + `accept()`
3. 測試失敗時明確報錯，不靜默通過
4. DPDK 設備不可用時測試**必須失敗**，不能跳過

### 任務失敗條件

如果任何測試（舊測試或新測試）滿足以下條件，整個任務視為失敗：
- 使用了 `tokio::net::TcpStream` 或 `tokio::net::TcpListener`
- 在 DPDK 不可用時回退到其他網路
- 未實際透過 smoltcp 發送/接收封包

---

## 技術決策記錄

### 已確認（不實現）
- `from_std` / `into_std` — DPDK 用戶態無 std socket 概念
- `linger` / `set_linger` — smoltcp 不支援 SO_LINGER
- `reuseaddr` / `reuseport` — 用戶態協議棧無意義
- `tos_v4` / `tclass_v6` — smoltcp 不支援
- `bind_device` — DPDK 已綁定設備

### 已決定（代理選擇）
- **DNS 解析**：使用現有 `crate::net::to_socket_addrs`（blocking pool）
- **nodelay 語義**：smoltcp 用 `nagle_enabled`，語義相反
- **TTL 對應**：smoltcp 用 `hop_limit`
- **quickack 對應**：smoltcp 用 `ack_delay`（None = 立即 ACK）
- **TcpDpdkSocket**：藍圖模式，配置保存在結構體，connect/listen 時應用

---

## 實現計劃

### 模組 0：測試修復（優先執行）

**必須首先完成測試修復，再實現新 API。**

#### 0.1 修復 `tcp_dpdk.rs`

**需要修改的測試模組**（`tcp_dpdk.rs` 內）：

| 模組/測試 | 行號 | 當前狀態 | 需要修改 |
|-----------|------|----------|----------|
| `mod api_parity` | L56-232 | 全用標準 TcpStream | ✅ 改用 TcpDpdkStream |
| `mod listener_tests` | L238-302 | 全用標準 TcpListener | ✅ 改用 TcpDpdkListener |
| `mod error_handling` | L434-467 | 用標準 TcpStream | ✅ 改用 TcpDpdkStream |
| `mod shutdown_tests` | L473-497 | 用標準 TcpListener | ✅ 改用 TcpDpdkListener |
| `mod stream_property_tests` | L564-693 | 混用 | ✅ 全改 DPDK 類型 |
| `mod worker_affinity_tests` | L701-761 | 只用標準 TcpStream | ✅ 改用 TcpDpdkStream |

**執行過程**：

首先修改 import 語句，將 `use tokio::net::{TcpListener, TcpStream}` 改為 `use tokio::net::{TcpDpdkListener, TcpDpdkStream}`。

接著修改每個測試函數：
- `TcpStream::connect(addr)` → `TcpDpdkStream::connect(addr)`
- `TcpListener::bind(addr)` → `TcpDpdkListener::bind(addr)`
- `listener.accept()` → `listener.accept()` (API 相同)
- `stream.into_split()` → `stream.into_split()` (已實現)

注意連接外部地址（如 127.0.0.1）需要 DPDK 路由配置支援。若無法連接 localhost，改為連接可路由的外部地址或使用 loopback 模式。

#### 0.2 修復 `dpdk_worker_isolation.rs`

**當前架構**（L1-14 註釋）：
```
標準 tokio TCP echo server（非 DPDK）
        ↓ 127.0.0.1
DPDK 客戶端使用標準 TcpStream  ← 錯誤！應使用 TcpDpdkStream
```

**正確架構**：
```
TcpDpdkListener echo server（DPDK 網路棧）
        ↓ smoltcp 內部或 DPDK 路由
TcpDpdkStream 客戶端（DPDK 網路棧）
```

**執行過程**：

修改 `start_echo_server` 函數（L43-98），使用 `TcpDpdkListener` 而非標準 `TcpListener`。

修改 `handle_client` 函數（L100-123），接收參數改為 `TcpDpdkStream`。

修改 `run_client_n_messages` 函數（L434-517），使用 `TcpDpdkStream::connect`。

整個測試架構需重新設計，確保服務端和客戶端都使用 DPDK 網路棧，避免跨網路棧通訊問題。

#### 0.3 修復 `tcp_dpdk_real.rs`

**問題**（L23）：
```rust
use tokio::net::TcpStream;  // 標準 TcpStream！
```

**需要修改的函數**：
- `subtest_ipv4_connect` (L167-205) — `TcpStream::connect` → `TcpDpdkStream::connect`
- `subtest_ipv4_read_write` (L207-235) — 同上
- `subtest_ipv6_connect` (L237-278) — 同上
- `subtest_many_connections` (L280-321) — 同上
- `subtest_multi_worker` (L323-367) — 同上

**執行過程**：

將 `use tokio::net::TcpStream` 改為 `use tokio::net::TcpDpdkStream`。

修改所有 `TcpStream::connect(...)` 為 `TcpDpdkStream::connect(...)`。

這個文件連接的是真實的 Cloudflare 服務器（1.1.1.1:80），需要確保 DPDK 路由配置正確。

#### 0.4 修復 `dpdk_multi_process.rs`

**問題行號**：L315, L344, L378, L408, L451, L490

這些行都使用 `TcpStream::connect("1.1.1.1:80")` 連接 Cloudflare。

**執行過程**：

將所有 `use tokio::net::TcpStream` 改為 `use tokio::net::TcpDpdkStream`。

修改所有 `TcpStream::connect(...)` 為 `TcpDpdkStream::connect(...)`。

---

### 模組 1：TcpDpdkStream 補齊

#### 1.1 DNS 解析：`connect<A: ToSocketAddrs>`

**簽名**：
```rust
pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<Self>
```

**語義**（參考 `tcp/stream.rs:L114-139`）：
解析地址（可能產生多個），依序嘗試連接，第一個成功即返回。所有失敗則返回最後一個錯誤。

**執行過程**：
將現有 `connect(SocketAddr)` 重命名為 `connect_addr`。新的 `connect` 方法調用 `to_socket_addrs` 解析後遍歷結果調用 `connect_addr`。

---

#### 1.2 修復：`nodelay` / `set_nodelay`

**現狀**（`stream.rs:L213-226`）：
```rust
pub fn set_nodelay(&self, _nodelay: bool) -> io::Result<()> {
    Ok(())  // no-op！
}
pub fn nodelay(&self) -> io::Result<bool> {
    Ok(true)  // 寫死！
}
```

**正確實現**：
smoltcp 支援 `set_nagle_enabled(bool)` 和 `nagle_enabled()`。修改為實際調用 smoltcp API，注意語義相反（nodelay=true 對應 nagle_enabled=false）。

---

#### 1.3 就緒等待：`ready`, `readable`, `writable`

**簽名**：
```rust
pub async fn ready(&self, interest: Interest) -> io::Result<Ready>
pub async fn readable(&self) -> io::Result<()>
pub async fn writable(&self) -> io::Result<()>
```

**語義**（參考 `tcp/stream.rs:L392-523, L786-835`）：
等待 socket 達到指定的就緒狀態。可能存在假陽性。Cancel safe。

**執行過程**：
基於現有 `poll_read_ready` / `poll_write_ready` 封裝 async 方法。`readable` 和 `writable` 是 `ready` 的簡寫。需要引入 `Interest` 和 `Ready` 類型。

---

#### 1.4 非阻塞讀寫：`try_read`, `try_write`

**簽名**：
```rust
pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize>
pub fn try_write(&self, buf: &[u8]) -> io::Result<usize>
```

**語義**（參考 `tcp/stream.rs:L558-627, L870-924`）：
嘗試立即讀寫，不等待。成功返回字節數，無數據/緩衝區滿返回 WouldBlock，連接關閉返回 0。

**執行過程**：
調用 smoltcp 的 `recv_slice` / `send_slice`。無數據時返回 WouldBlock 並清除 readiness。

**可選功能**：`try_read_buf<B: BufMut>` 需要 `io_util` feature，與 `try_read` 類似但使用 `BufMut` trait。

---

#### 1.5 向量化 I/O：`try_read_vectored`, `try_write_vectored`

**簽名**：
```rust
pub fn try_read_vectored(&self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize>
pub fn try_write_vectored(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize>
```

**語義**：
依序讀寫多個 buffer，返回總字節數。

**執行過程**：
smoltcp 不支援原生 vectored I/O。透過循環調用 `recv_slice` / `send_slice` 實現。遍歷每個 buffer，累計字節數直到遇到 WouldBlock 或填滿/寫完。

---

#### 1.6 Peek：`peek`, `poll_peek`

**簽名**：
```rust
pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize>
pub fn poll_peek(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<usize>>
```

**語義**（參考 `tcp/stream.rs:L324-390, L1066-1115`）：
讀取數據但不從接收緩衝區移除，後續 read 會再次看到相同數據。Cancel safe。

**執行過程**：
smoltcp 原生支援 `socket.peek(size) -> &[u8]`。等待 readable 後調用 peek 並複製到 buf。

---

#### 1.7 TTL：`ttl`, `set_ttl`

**簽名**：
```rust
pub fn ttl(&self) -> io::Result<u32>
pub fn set_ttl(&self, ttl: u32) -> io::Result<()>
```

**語義**：IP 封包的 TTL 值。

**執行過程**：
smoltcp 用 `hop_limit()` / `set_hop_limit(Some(u8))`。

---

#### 1.8 Quickack：`quickack`, `set_quickack` (Linux)

**簽名**：
```rust
#[cfg(target_os = "linux")]
pub fn quickack(&self) -> io::Result<bool>
#[cfg(target_os = "linux")]
pub fn set_quickack(&self, quickack: bool) -> io::Result<()>
```

**語義**：禁用 delayed ACK。

**執行過程**：
smoltcp 用 `ack_delay()`。設為 None = 立即 ACK = quickack。設為 Some(duration) = 延遲 ACK。

---

#### 1.9 錯誤追蹤：`take_error`

**簽名**：
```rust
pub fn take_error(&self) -> io::Result<Option<io::Error>>
```

**執行過程**：
smoltcp 無直接對應。檢查 socket state，若為異常狀態返回對應錯誤。需在 TcpDpdkStream 中新增欄位追蹤。

---

#### 1.10 關閉：`shutdown_std`

**簽名**：
```rust
pub(super) fn shutdown_std(&self, how: Shutdown) -> io::Result<()>
```

**語義**（參考 `tcp/stream.rs:L1117-1132`）：
- `Shutdown::Write`：關閉自己的寫端，發送 FIN
- `Shutdown::Read`：關閉自己的讀端，後續 read 返回 EOF
- `Shutdown::Both`：兩者都做
- NotConnected 錯誤轉換為 Ok(())

**執行過程**：
對於 Write：調用 smoltcp `close()`。
對於 Read：smoltcp 不支援。設置內部 `read_shutdown` 標誌，後續 read 返回 Ok(0)。
需在 TcpDpdkStream 中新增 `read_shutdown: bool` 欄位。

---

#### 1.11 通用 I/O：`try_io`, `async_io`

**簽名**：
```rust
pub fn try_io<R>(&self, interest: Interest, f: impl FnOnce() -> io::Result<R>) -> io::Result<R>
pub async fn async_io<R>(&self, interest: Interest, f: impl FnMut() -> io::Result<R>) -> io::Result<R>
```

**語義**（參考 `tcp/stream.rs:L988-1064`）：
`try_io`：就緒時執行閉包，WouldBlock 時清除 readiness。
`async_io`：循環等待就緒並執行，直到非 WouldBlock。

**執行過程**：
基於 `ready()` 和 readiness 管理邏輯封裝。

---

### 模組 2：TcpDpdkListener 補齊

#### 2.1 DNS 解析：`bind<A: ToSocketAddrs>`

與 TcpDpdkStream::connect 類似，使用 `to_socket_addrs` 解析地址。

#### 2.2 TTL：`ttl`, `set_ttl`

與 TcpDpdkStream 相同實現。

---

### 模組 3：新類型 TcpDpdkSocket

#### 3.1 結構定義

```rust
/// TCP socket 配置器，用於在連接或監聽前設置選項。
/// 語義：藍圖模式 — 配置保存在結構體，connect/listen 時才創建 smoltcp socket。
pub struct TcpDpdkSocket {
    domain: IpVersion,
    local_addr: Option<SocketAddr>,
    rx_buffer_size: usize,
    tx_buffer_size: usize,
    nodelay: bool,
    hop_limit: Option<u8>,
    keep_alive: Option<Duration>,
    ack_delay: Option<Duration>,
}
```

#### 3.2 創建方法

```rust
pub fn new_v4() -> io::Result<TcpDpdkSocket>
pub fn new_v6() -> io::Result<TcpDpdkSocket>
```

返回帶預設配置的 TcpDpdkSocket（buffer size = 65536，nodelay = false）。

#### 3.3 配置方法

```rust
pub fn bind(&self, addr: SocketAddr) -> io::Result<()>
pub fn local_addr(&self) -> io::Result<SocketAddr>
pub fn set_send_buffer_size(&self, size: u32) -> io::Result<()>
pub fn send_buffer_size(&self) -> io::Result<u32>
pub fn set_recv_buffer_size(&self, size: u32) -> io::Result<()>
pub fn recv_buffer_size(&self) -> io::Result<u32>
pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()>
pub fn nodelay(&self) -> io::Result<bool>
pub fn set_keepalive(&self, keepalive: bool) -> io::Result<()>
pub fn keepalive(&self) -> io::Result<bool>
pub fn set_keepalive_interval(&self, interval: Duration) -> io::Result<()>
pub fn keepalive_interval(&self) -> io::Result<Option<Duration>>
```

使用內部可變性保存配置。

**注意**：`TcpDpdkSocket` 不協助 DNS 解析 — `bind` 和 `connect` 只接受 `SocketAddr`。`listen` 是同步方法。

#### 3.4 連接/監聯方法

```rust
pub async fn connect(self, addr: SocketAddr) -> io::Result<TcpDpdkStream>
pub fn listen(self, backlog: u32) -> io::Result<TcpDpdkListener>
```

消耗 self，使用配置創建 smoltcp socket（調用 DpdkDriver 的新方法 `create_tcp_socket_with_config`），應用選項，執行 connect/listen。

---

### 模組 4：DpdkDriver 擴展

#### 4.1 帶配置的 socket 創建

```rust
pub(crate) fn create_tcp_socket_with_config(
    &mut self,
    rx_size: usize,
    tx_size: usize,
) -> Option<SocketHandle>
```

若 size 與預設相同則使用 pool，否則動態分配。

---

### 模組 5：文檔更新

實現完成後更新：
- `TOKIO_DPDK_GUIDE.md` — 新增 TcpDpdkSocket 範例、API 對比表
- 各模組 doc comments

---

## 驗收標準

### 測試修復（最高優先級）

- [x] **AC-0.1** [mandatory] `tcp_dpdk.rs` 中所有 TCP 測試使用 `TcpDpdkStream` / `TcpDpdkListener`
- [x] **AC-0.2** [mandatory] `tcp_dpdk_real.rs` 使用 `TcpDpdkStream` (6/6 測試通過)
- [x] **AC-0.3** [mandatory] `dpdk_multi_process.rs` 使用 `TcpDpdkStream`
- [x] **AC-0.4** [mandatory] `dpdk_worker_isolation.rs` 客戶端使用 `TcpDpdkStream`（服務端使用標準 TCP 是允許的）
- [x] **AC-0.5** [test] `tcp_dpdk_real.rs` 所有 6 個測試在真實 DPDK 環境通過
- [x] **AC-0.6** [manual] 代碼審查：已確認 DPDK 測試文件客戶端使用 `TcpDpdkStream`


### TcpDpdkStream 新功能

- [x] **AC-1** [test] `test_connect_with_socket_addr` — SocketAddr 連接成功
- [x] **AC-2** [test] `test_connect_with_hostname` — hostname 連接成功
- [x] **AC-3** [test] `test_nodelay_actually_works` — set/get nodelay 實際生效
- [x] **AC-4** [test] `test_ready_readable` — readable() 等待數據
- [x] **AC-5** [test] `test_try_read_would_block` — 無數據返回 WouldBlock
- [x] **AC-6** [test] `test_try_read_success` — 有數據成功讀取
- [x] **AC-7** [test] `test_peek_data` — peek 不消費數據
- [x] **AC-8** [test] `test_set_ttl` — set/get TTL
- [x] **AC-9** [test] `test_shutdown_write` — 關閉寫端

### TcpDpdkSocket

- [x] **AC-10** [test] `test_socket_new_v4` — 創建成功
- [x] **AC-11** [test] `test_socket_bind_connect` — 綁定後連接使用指定本地地址
- [x] **AC-12** [test] `test_socket_buffer_size` — 配置 buffer size

### TcpDpdkListener

- [x] **AC-13** [test] `test_listener_bind_hostname` — hostname 綁定
- [x] **AC-14** [test] `test_listener_ttl` — set/get TTL

### 編譯檢查

- [x] **AC-15** [build] cargo check 無錯誤
- [x] **AC-16** [build] cargo check 無新增警告

### 文檔

- [x] **AC-17** [manual] TOKIO_DPDK_GUIDE.md 已更新
- [x] **AC-18** [manual] 所有新方法有 doc comments

---

## 注意事項

1. **模組 0 優先**：必須先完成測試修復，確認 DPDK 網路棧可用，再實現新 API

2. **Worker 親和性**：所有方法必須在正確的 worker 調用，保留 `assert_on_correct_worker()` 檢查

3. **smoltcp 語義差異**：
   - `nagle_enabled` 與 `nodelay` 語義相反
   - `hop_limit` 對應 TTL
   - `ack_delay` 對應 quickack（None = quickack）
   - 無法主動關閉讀端（使用標誌模擬）

4. **AsyncWrite trait**：預設已提供 `poll_write_vectored`（透過循環調用 poll_write），`is_write_vectored` 預設返回 false，無需額外實現

5. **API 簽名一致性**：所有新方法簽名必須與標準 tokio 一致

6. **測試必須在 DPDK 環境運行**：這是強制要求，不是可選條件。若 DPDK 環境不可用，任務無法完成。不接受「環境不可用所以跳過測試修復」的理由。

---

*創建時間：2026-01-11*
*基於代碼審查：tcp_dpdk.rs, tcp_dpdk_real.rs, dpdk_multi_process.rs, dpdk_worker_isolation.rs*

