# TcpDpdkStream API 對等性分析

本文件分析標準 tokio TCP API 在 DPDK/smoltcp 環境下的實現可行性。

**研究來源**：
- smoltcp 0.11.0 文檔：https://docs.rs/smoltcp/0.11.0/smoltcp/socket/tcp/struct.Socket.html
- tokio-dpdk 本地代碼：`tokio/src/runtime/scheduler/dpdk/dpdk_driver.rs`
- tokio/src/net/tcp/* 標準實現

---

## 可行性評估標準

| 評級 | 說明 |
|------|------|
| ✅ **已實現** | 當前 TcpDpdkStream 已有對應功能 |
| 🟢 **簡單** | smoltcp 直接支援，僅需封裝 |
| 🟡 **中等** | 需要額外邏輯或狀態管理 |
| 🟠 **困難** | 需要重大架構變更或 smoltcp 不完整支援 |
| 🔴 **不可行** | smoltcp 不支援或與 DPDK 架構根本衝突 |
| ⬛ **不適用** | 與 DPDK 用戶態模式概念不相容 |

---

## TcpStream 方法對比

### 連接/創建

| ✓ | 方法 | 標準 TcpStream | TcpDpdkStream | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|---------------|--------------|--------|------|
| [x] | `connect<A: ToSocketAddrs>(addr)` | ✅ | ❌ 只有 `connect(SocketAddr)` | N/A (DNS 在上層) | 🟢 簡單 | 使用現有 `to_socket_addrs` 在 blocking pool 執行 DNS |
| [ ] | `from_std(std::TcpStream)` | ✅ | ❌ | N/A | ⬛ 不適用 | DPDK 繞過內核，無 std socket 概念 |
| [ ] | `into_std()` | ✅ | ❌ | N/A | ⬛ 不適用 | 同上 |

### 地址資訊

| ✓ | 方法 | 標準 TcpStream | TcpDpdkStream | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|---------------|--------------|--------|------|
| [x] | `local_addr()` | ✅ | ✅ | `local_endpoint()` | ✅ 已實現 | — |
| [x] | `peer_addr()` | ✅ | ✅ | `remote_endpoint()` | ✅ 已實現 | — |
| [x] | `take_error()` | ✅ | ❌ | `state()` 可推斷 | 🟡 中等 | 需追蹤 socket 狀態變化來推斷錯誤 |

### 就緒輪詢（Readiness）

| ✓ | 方法 | 標準 TcpStream | TcpDpdkStream | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|---------------|--------------|--------|------|
| [x] | `ready(Interest)` | ✅ | ❌ | `can_send()`, `can_recv()` | 🟢 簡單 | 基於 ScheduledIo 封裝 async 等待 |
| [x] | `readable()` | ✅ | ❌ | `can_recv()` | 🟢 簡單 | `ready(Interest::READABLE)` 的簡寫 |
| [x] | `writable()` | ✅ | ❌ | `can_send()` | 🟢 簡單 | `ready(Interest::WRITABLE)` 的簡寫 |
| [x] | `poll_read_ready(cx)` | ✅ | ✅ | ScheduledIo | ✅ 已實現 | — |
| [x] | `poll_write_ready(cx)` | ✅ | ✅ | ScheduledIo | ✅ 已實現 | — |

### 非阻塞讀寫（Try Methods）

| ✓ | 方法 | 標準 TcpStream | TcpDpdkStream | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|---------------|--------------|--------|------|
| [x] | `try_read(&mut buf)` | ✅ | ❌ | `recv_slice()` | 🟢 簡單 | 直接調用 smoltcp，返回 WouldBlock 如果沒數據 |
| [x] | `try_write(&buf)` | ✅ | ❌ | `send_slice()` | 🟢 簡單 | 同上 |
| [x] | `try_read_vectored()` | ✅ | ❌ | ❌ 無原生支援 | 🟡 中等 | 需多次 `recv_slice()` 填充多個 buffer |
| [x] | `try_write_vectored()` | ✅ | ❌ | ❌ 無原生支援 | 🟡 中等 | 需多次 `send_slice()` 寫入多個 buffer |
| [x] | `try_read_buf()` | ✅ | ❌ | `recv_slice()` | 🟢 簡單 | 與 try_read 類似，只是用 BufMut |

### Peek（查看但不消費數據）

| ✓ | 方法 | 標準 TcpStream | TcpDpdkStream | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|---------------|--------------|--------|------|
| [x] | `peek(&mut buf)` | ✅ | ❌ | `peek(size) -> &[u8]` | 🟢 簡單 | smoltcp 原生支援 peek 方法 |
| [x] | `poll_peek(cx, buf)` | ✅ | ❌ | `peek()` + waker | 🟢 簡單 | 結合 ScheduledIo 和 smoltcp peek |

### Socket 選項

| ✓ | 方法 | 標準 TcpStream | TcpDpdkStream | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|---------------|--------------|--------|------|
| [x] | `nodelay()` | ✅ | ⚠️ (no-op) | `nagle_enabled()` | 🟢 簡單 | smoltcp 支援 `set_nagle_enabled(bool)` |
| [x] | `set_nodelay()` | ✅ | ⚠️ (no-op) | `set_nagle_enabled()` | 🟢 簡單 | 同上 |
| [x] | `ttl()` | ✅ | ❌ | `hop_limit()` | 🟢 簡單 | smoltcp 用 `hop_limit` 表示 TTL |
| [x] | `set_ttl()` | ✅ | ❌ | `set_hop_limit()` | 🟢 簡單 | 同上 |
| [x] | `quickack()` | ✅ (Linux) | ❌ | `ack_delay()` | 🟢 簡單 | smoltcp 用 ack_delay，設為 None = 立即 ACK |
| [x] | `set_quickack()` | ✅ (Linux) | ❌ | `set_ack_delay()` | 🟢 簡單 | 同上 |
| [x] | `linger()` | ✅ | ❌ | ❌ 不支援 | 🔴 不可行 | smoltcp 無 SO_LINGER 概念 |
| [x] | `set_linger()` | ✅ | ❌ | ❌ 不支援 | 🔴 不可行 | 同上 |

### 分割（Split）

| ✓ | 方法 | 標準 TcpStream | TcpDpdkStream | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|---------------|--------------|--------|------|
| [x] | `split()` | ✅ | ✅ | N/A (應用層封裝) | ✅ 已實現 | — |
| [x] | `into_split()` | ✅ | ✅ | N/A | ✅ 已實現 | — |

### 關閉

| ✓ | 方法 | 標準 TcpStream | TcpDpdkStream | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|---------------|--------------|--------|------|
| [x] | `shutdown_std(how)` | ✅ | ❌ | `close()`, `abort()` | 🟡 中等 | smoltcp `close()` 關閉寫端，`abort()` 強制關閉；讀端只能由遠端關閉 |

### 通用 I/O

| ✓ | 方法 | 標準 TcpStream | TcpDpdkStream | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|---------------|--------------|--------|------|
| [x] | `try_io(interest, f)` | ✅ | ❌ | N/A | 🟡 中等 | 需要封裝 readiness 檢查邏輯 |
| [x] | `async_io(interest, f)` | ✅ | ❌ | N/A | 🟡 中等 | 類似 try_io 但帶 async 等待 |

### AsyncRead/AsyncWrite Trait

| ✓ | 方法 | 標準 TcpStream | TcpDpdkStream | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|---------------|--------------|--------|------|
| [x] | `poll_read` | ✅ | ✅ | `recv_slice()` | ✅ 已實現 | — |
| [x] | `poll_write` | ✅ | ✅ | `send_slice()` | ✅ 已實現 | — |
| [x] | `poll_flush` | ✅ | ✅ | N/A (driver poll 處理) | ✅ 已實現 | — |
| [x] | `poll_shutdown` | ✅ | ✅ | `close()` | ✅ 已實現 | — |
| [x] | `poll_write_vectored` | ✅ | ❌ | ❌ 無原生支援 | 🟡 中等 | 需循環調用 send_slice |
| [x] | `is_write_vectored` | ✅ | ❌ | — | 🟢 簡單 | 返回 false（不原生支援） |

### DPDK 專用方法

| ✓ | 方法 | TcpDpdkStream | 說明 |
|---|------|---------------|------|
| [x] | `core_id()` | ✅ | 返回 socket 綁定的 worker core ID |

---

## TcpSocket（新類型：TcpDpdkSocket）

TcpSocket 是連接前的配置器（藍圖語義），允許設置 socket 選項後再連接。

| ✓ | 方法 | 標準 TcpSocket | smoltcp 支援 | 可行性 | 說明 |
|---|------|---------------|--------------|--------|------|
| [x] | `new_v4()` / `new_v6()` | ✅ | 創建配置結構 | 🟡 中等 | 需要新的類型管理未連接的 socket 狀態 |
| [x] | `bind(addr)` | ✅ | 保存 local_endpoint | 🟢 簡單 | 藍圖模式：保存配置，connect 時應用 |
| [x] | `connect(addr)` | ✅ | `connect()` | 🟡 中等 | 消耗 socket，返回 TcpDpdkStream |
| [x] | `listen(backlog)` | ✅ | `listen()` | 🟡 中等 | 消耗 socket，返回 TcpDpdkListener |
| [x] | `set_reuseaddr()` | ✅ | ❌ 不適用 | ⬛ 不適用 | 用戶態協議棧無 SO_REUSEADDR 概念 |
| [x] | `set_reuseport()` | ✅ | ❌ 不適用 | ⬛ 不適用 | 同上 |
| [x] | `set_keepalive()` | ✅ | `set_keep_alive()` | 🟢 簡單 | smoltcp 原生支援 |
| [x] | `keepalive()` | ✅ | `keep_alive()` | 🟢 簡單 | 同上 |
| [x] | `set_send_buffer_size()` | ✅ | 藍圖保存，connect 時應用 | 🟢 簡單 | TcpDpdkSocket 保存配置，創建 smoltcp socket 時使用 |
| [x] | `send_buffer_size()` | ✅ | `send_capacity()` | 🟢 簡單 | — |
| [x] | `set_recv_buffer_size()` | ✅ | 藍圖保存，connect 時應用 | 🟢 簡單 | TcpDpdkSocket 保存配置，創建 smoltcp socket 時使用 |
| [x] | `recv_buffer_size()` | ✅ | `recv_capacity()` | 🟢 簡單 | — |
| [x] | `set_linger()` | ✅ | ❌ 不支援 | 🔴 不可行 | smoltcp 無 SO_LINGER |
| [x] | `linger()` | ✅ | ❌ 不支援 | 🔴 不可行 | 同上 |
| [x] | `set_nodelay()` | ✅ | `set_nagle_enabled()` | 🟢 簡單 | — |
| [x] | `nodelay()` | ✅ | `nagle_enabled()` | 🟢 簡單 | — |
| [x] | `set_tos_v4()` | ✅ | ❌ 不支援 | 🔴 不可行 | smoltcp 無 TOS/DSCP 設置 |
| [x] | `set_tclass_v6()` | ✅ | ❌ 不支援 | 🔴 不可行 | 同上 |
| [x] | `bind_device()` | ✅ | N/A | ⬛ 不適用 | DPDK 環境每個 worker 已綁定特定 device |
| [x] | `local_addr()` | ✅ | `local_endpoint()` | 🟢 簡單 | — |
| [x] | `take_error()` | ✅ | 狀態推斷 | 🟡 中等 | — |

---

## TcpListener 對比

| ✓ | 方法 | 標準 TcpListener | TcpDpdkListener | smoltcp 支援 | 可行性 | 說明 |
|---|------|-----------------|-----------------|--------------|--------|------|
| [x] | `bind<A: ToSocketAddrs>(addr)` | ✅ | ❌ 只有 `bind(SocketAddr)` | N/A | 🟢 簡單 | 與 connect 相同，使用 to_socket_addrs |
| [x] | `accept()` | ✅ | ✅ | — | ✅ 已實現 | — |
| [x] | `local_addr()` | ✅ | ✅ | — | ✅ 已實現 | — |
| [x] | `ttl()` / `set_ttl()` | ✅ | ❌ | `hop_limit()` | 🟢 簡單 | — |

---

## 優先級建議

### Phase 1：高優先級（核心 API 對等）
| ✓ | 項目 |
|---|------|
| [x] | `connect<A: ToSocketAddrs>` — DNS 解析支援 |
| [x] | `try_read()` / `try_write()` — 非阻塞讀寫 |
| [x] | `peek()` / `poll_peek()` — 查看數據 |
| [x] | `ready()` / `readable()` / `writable()` — async 就緒等待 |
| [x] | 修復 `set_nodelay()` — smoltcp 已支援 |

### Phase 2：中優先級（完整選項）
| ✓ | 項目 |
|---|------|
| [x] | `TcpDpdkSocket` — 支援指定本地地址連接 |
| [x] | `ttl()` / `set_ttl()` — TTL 設置 |
| [x] | `quickack()` — ACK 延遲設置 |
| [x] | `keepalive()` — 保活設置 |
| [x] | `shutdown_std(how)` — 半關閉 |

### Phase 3：低優先級（進階功能）
| ✓ | 項目 |
|---|------|
| [x] | `try_read_vectored()` / `try_write_vectored()` — 向量化 I/O |
| [x] | `poll_write_vectored()` — 向量化寫入 |
| [x] | `try_io()` / `async_io()` — 通用 I/O 封裝 |
| [x] | `take_error()` — 錯誤追蹤 |

### 不實現（與 DPDK/smoltcp 不相容）
| ✓ | 項目 |
|---|------|
| [x] | `from_std()` / `into_std()` — 無 std socket 概念 |
| [x] | `set_linger()` / `linger()` — smoltcp 不支援 |
| [x] | `set_reuseaddr()` / `set_reuseport()` — 用戶態無意義 |
| [x] | `set_tos_v4()` / `set_tclass_v6()` — smoltcp 不支援 |
| [x] | `bind_device()` — DPDK 已綁定設備 |

---

## smoltcp 0.11.0 TCP Socket 關鍵 API 參考

```rust
// 連接與監聽
fn connect(&mut self, cx: &mut Context, remote: IpEndpoint, local: IpListenEndpoint) -> Result<(), ConnectError>
fn listen(&mut self, endpoint: IpListenEndpoint) -> Result<(), ListenError>
fn close(&mut self)
fn abort(&mut self)

// 狀態查詢
fn state(&self) -> State
fn is_open(&self) -> bool
fn is_active(&self) -> bool
fn can_send(&self) -> bool
fn can_recv(&self) -> bool
fn may_send(&self) -> bool
fn may_recv(&self) -> bool

// 地址
fn local_endpoint(&self) -> Option<IpEndpoint>
fn remote_endpoint(&self) -> Option<IpEndpoint>

// 讀寫
fn send_slice(&mut self, data: &[u8]) -> Result<usize, SendError>
fn recv_slice(&mut self, data: &mut [u8]) -> Result<usize, RecvError>
fn peek(&mut self, size: usize) -> Result<&[u8], RecvError>
fn peek_slice(&mut self, data: &mut [u8]) -> Result<usize, RecvError>

// Socket 選項
fn set_nagle_enabled(&mut self, enabled: bool)  // TCP_NODELAY
fn nagle_enabled(&self) -> bool
fn set_hop_limit(&mut self, hop_limit: Option<u8>)  // TTL
fn hop_limit(&self) -> Option<u8>
fn set_timeout(&mut self, duration: Option<Duration>)
fn timeout(&self) -> Option<Duration>
fn set_keep_alive(&mut self, interval: Option<Duration>)
fn keep_alive(&self) -> Option<Duration>
fn set_ack_delay(&mut self, duration: Option<Duration>)
fn ack_delay(&self) -> Option<Duration>

// Buffer 資訊
fn send_capacity(&self) -> usize
fn recv_capacity(&self) -> usize
fn send_queue(&self) -> usize
fn recv_queue(&self) -> usize

// Waker 註冊
fn register_recv_waker(&mut self, waker: &Waker)
fn register_send_waker(&mut self, waker: &Waker)
```

---

*最後更新：2026-01-11*
