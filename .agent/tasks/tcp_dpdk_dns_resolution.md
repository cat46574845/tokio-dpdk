# TcpDpdkStream DNS 解析支援

## 背景與目的

### 目的
讓 `TcpDpdkStream::connect` 和 `TcpDpdkListener::bind` 方法支援 DNS 解析，使用戶可以直接傳入域名字串（如 `"example.com:8080"`）而非只能使用 `SocketAddr`。

### 項目上下文
`tokio-dpdk` 是一個 Tokio fork，新增了 DPDK 調度器以實現超低延遲網路。目前 `TcpDpdkStream` 和 `TcpDpdkListener` 的 API 只接受 `SocketAddr` 類型，這與標準 tokio 的 `TcpStream::connect<A: ToSocketAddrs>(addr: A)` 不一致，降低了 API 的易用性。

標準 tokio 的 DNS 解析機制：
1. 使用 `ToSocketAddrs` trait 抽象地址解析
2. 對於字串類型，先嘗試解析為 IP 地址，如失敗則透過 `spawn_blocking` 在阻塞線程池執行 `std::net::ToSocketAddrs::to_socket_addrs`
3. 如果解析出多個地址，依序嘗試連接直到成功

### 相關文件
- `tokio/src/net/tcp_dpdk/stream.rs` — TcpDpdkStream 實現，包含 `connect` 方法
- `tokio/src/net/tcp_dpdk/listener.rs` — TcpDpdkListener 實現，包含 `bind` 方法
- `tokio/src/net/tcp_dpdk/mod.rs` — tcp_dpdk 模組入口
- `tokio/src/net/addr.rs` — `ToSocketAddrs` trait 和 `to_socket_addrs` 函數定義
- `tokio/src/net/tcp/stream.rs:L115-133` — 標準 TcpStream::connect 的實現範例

---

## 技術決策記錄

### 已確認
- **DNS 解析機制**: 使用現有的 `crate::net::to_socket_addrs` 函數 — 與標準 tokio 保持一致，複用現有基礎設施

### 已決定（代理選擇）
- **多地址處理策略**: 依序嘗試連接直到成功 — 與標準 `TcpStream::connect` 行為一致 (參考 `tokio/src/net/tcp/stream.rs:L120-132`)
- **原始 `connect(SocketAddr)` 保留**: 新增 `connect_addr` 作為內部方法，原 `connect` 改為泛型版本 — 保持向後兼容
- **Listener bind 同時支援**: 一併修改 `TcpDpdkListener::bind` 支援 `ToSocketAddrs` — 保持 API 一致性

---

## 實現計劃

### 修改 1: `stream.rs` — TcpDpdkStream::connect 泛型化

**目的**: 將 `connect` 方法改為接受任何實現 `ToSocketAddrs` 的類型

**簽名變更**:
```rust
// 舊簽名
pub async fn connect(addr: SocketAddr) -> io::Result<Self>

// 新簽名
pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<Self>
```

**執行過程**:

首先在文件頂部新增必要的 import：
```rust
use crate::net::{to_socket_addrs, ToSocketAddrs};
```

接著將現有的 `connect` 方法重命名為 `connect_addr`，並保持其簽名和實現不變。這個方法處理單一 `SocketAddr` 的連接邏輯。

然後新增泛型版本的 `connect` 方法：

```rust
/// Connect to a remote address.
///
/// `addr` is an address of the remote host. Anything which implements the
/// [`ToSocketAddrs`] trait can be supplied as the address. If `addr`
/// yields multiple addresses, connect will be attempted with each of the
/// addresses until a connection is successful.
///
/// # Example
///
/// ```ignore
/// use tokio::net::TcpDpdkStream;
/// 
/// // Using SocketAddr directly
/// let stream = TcpDpdkStream::connect("1.2.3.4:8080".parse().unwrap()).await?;
/// 
/// // Using hostname (requires DNS resolution)
/// let stream = TcpDpdkStream::connect("example.com:8080").await?;
/// ```
///
/// [`ToSocketAddrs`]: trait@crate::net::ToSocketAddrs
pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<Self>
```

執行邏輯：
1. 調用 `to_socket_addrs(addr).await?` 解析地址（DNS 解析在 blocking pool 執行）
2. 遍歷所有解析出的地址
3. 對每個地址調用 `connect_addr` 嘗試連接
4. 如果連接成功，返回 stream
5. 如果失敗，記錄錯誤並嘗試下一個地址
6. 如果所有地址都失敗，返回最後一個錯誤
7. 如果沒有解析出任何地址，返回 "could not resolve to any address" 錯誤

---

### 修改 2: `listener.rs` — TcpDpdkListener::bind 泛型化

**目的**: 將 `bind` 方法改為接受任何實現 `ToSocketAddrs` 的類型

**簽名變更**:
```rust
// 舊簽名
pub async fn bind(addr: SocketAddr) -> io::Result<Self>

// 新簽名
pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self>
```

**執行過程**:

首先在文件頂部新增必要的 import：
```rust
use crate::net::{to_socket_addrs, ToSocketAddrs};
```

接著將現有的 `bind` 方法重命名為 `bind_addr`，保持其原有實現。

然後新增泛型版本的 `bind` 方法：

```rust
/// Binds to the specified address.
///
/// `addr` can be any type that implements [`ToSocketAddrs`], including
/// string slices like `"0.0.0.0:8080"` or `("localhost", 8080)`.
/// 
/// If `addr` yields multiple addresses, binding is attempted with each
/// until one succeeds.
///
/// # Example
///
/// ```ignore
/// use tokio::net::TcpDpdkListener;
/// 
/// // Using string address
/// let listener = TcpDpdkListener::bind("0.0.0.0:8080").await?;
/// 
/// // Using tuple
/// let listener = TcpDpdkListener::bind(("0.0.0.0", 8080)).await?;
/// ```
///
/// [`ToSocketAddrs`]: trait@crate::net::ToSocketAddrs
pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self>
```

執行邏輯與 connect 類似：
1. 調用 `to_socket_addrs(addr).await?` 解析地址
2. 遍歷所有解析出的地址
3. 對每個地址調用 `bind_addr` 嘗試綁定
4. 如果綁定成功，返回 listener
5. 如果失敗，記錄錯誤並嘗試下一個地址
6. 如果所有地址都失敗，返回最後一個錯誤
7. 如果沒有解析出任何地址，返回 "could not resolve to any address" 錯誤

---

### 修改 3: `mod.rs` — 更新模組文檔範例

**目的**: 更新文檔範例展示 DNS 解析功能

**執行過程**:

將 mod.rs 中的範例更新為展示字串地址用法：

```rust
//! # Usage
//!
//! ```ignore
//! use tokio::net::{TcpDpdkStream, TcpDpdkListener};
//!
//! // Client connection with hostname
//! let stream = TcpDpdkStream::connect("example.com:8080").await?;
//!
//! // Server listener
//! let listener = TcpDpdkListener::bind("0.0.0.0:8080").await?;
//! let (stream, addr) = listener.accept().await?;
//! ```
```

---

### 低級函數

- `connect_addr(addr: SocketAddr) -> io::Result<Self>` — 原 connect 邏輯，處理單一 SocketAddr 連接
- `bind_addr(addr: SocketAddr) -> io::Result<Self>` — 原 bind 邏輯，處理單一 SocketAddr 綁定

---

## 驗收標準

**編譯檢查**:
- [x] **AC-1** [build] `cargo check -p tokio --features full` 無錯誤
- [x] **AC-2** [build] `cargo check -p tokio --features full` 無新增警告

**自動化測試**:
- [x] **AC-3** [test] `test_connect_with_socket_addr` — 使用 SocketAddr 連接成功（向後兼容）
- [x] **AC-4** [test] `test_connect_with_string` — 使用字串地址 "1.2.3.4:8080" 連接成功
- [x] **AC-5** [test] `test_connect_with_hostname` — 使用主機名 "localhost:port" 連接成功
- [x] **AC-6** [test] `test_connect_invalid_hostname` — 無效主機名返回適當錯誤
- [x] **AC-7** [test] `test_bind_with_string` — 使用字串地址 "0.0.0.0:0" 綁定成功
- [x] **AC-8** [test] `test_bind_with_tuple` — 使用 tuple ("0.0.0.0", 0) 綁定成功

**人工驗證**:
- [x] **AC-9** [manual] 文檔範例正確更新並展示新 API 用法

---

## 注意事項

1. **DNS 解析發生在 blocking pool**：`to_socket_addrs` 對於字串類型會使用 `spawn_blocking`，這不會阻塞 DPDK worker。這是設計使然，與標準 tokio 行為一致。

2. **Worker 親和性不受影響**：DNS 解析在 blocking pool 完成後，實際的 TCP 連接仍在 DPDK worker 上執行。`connect_addr` 內部的 `current_worker_index()` 檢查確保這一點。

3. **連接建立仍返回立即**：當前 `connect_addr` 實現有一個 TODO 註解 "Wait for connection establishment"。這個任務不處理該問題，只專注於 DNS 解析支援。

4. **IPv6 仍未支援**：現有代碼對 IPv6 返回 `Unsupported` 錯誤。這個任務不改變該行為。

5. **Feature flag**: `ToSocketAddrs` 對字串的 DNS 支援需要 tokio 的 `dns` feature。確保測試時啟用 `full` feature。
