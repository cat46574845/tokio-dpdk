# DPDK 模塊生產就緒任務

## 任務目標

將 `tokio-dpdk` 的 DPDK scheduler 和 TCP 網絡達到生產品質：
- DPDK runtime 可創建並使用所有 tokio 功能
- TcpDpdkStream 行為與標準 TcpStream 一致
- 0 個編譯警告（從 root cause 修復）
- 通過 autobahn-testsuite WebSocket 測試

---

## 執行指引

**迭代執行模式**：

1. 按 Section 順序執行（1→2→3→4）
2. 每完成一個 Section，執行編譯檢查
3. 確認無新錯誤後繼續下一 Section

**編譯檢查命令**（Windows）：
```powershell
cd f:\tokio-dpdk
cargo check -p tokio --features rt-multi-thread 2>&1 | Out-File -FilePath cargo_errors.txt -Encoding utf8
# 確認 "generated 0 warnings"，然後刪除 cargo_errors.txt
```

---

## Section 1: 架構缺失修復（ROOT CAUSE）

### 1.1 問題：無法創建 DPDK Runtime

**Root Cause 分析**:

`Kind::Dpdk` 定義於 `builder.rs:237`，`build_dpdk_runtime()` 實現於 `builder.rs:1843-1920`，但：
- **沒有 `Builder::new_dpdk()` 公開方法**
- **沒有任何 API 將 `self.kind` 設置為 `Kind::Dpdk`**
- **結果**：所有 DPDK 相關代碼永遠無法執行 → 350 個 dead code 警告

**解決方案**:

在 `builder.rs` 添加 `Builder::new_dpdk()` 方法（在 `new_multi_thread()` 之後，約 L269）：

```rust
/// Returns a new builder with the DPDK scheduler selected.
///
/// This creates a runtime that uses DPDK for high-performance networking
/// with kernel bypass and busy-poll workers.
///
/// # Example
///
/// ```ignore
/// use tokio::runtime::Builder;
///
/// let rt = Builder::new_dpdk()
///     .dpdk_device("eth0")
///     .enable_io()
///     .enable_time()
///     .build()
///     .unwrap();
/// ```
#[cfg(feature = "rt-multi-thread")]
#[cfg_attr(docsrs, doc(cfg(feature = "rt-multi-thread")))]
pub fn new_dpdk() -> Builder {
    let mut builder = Builder::new(Kind::Dpdk, 61);
    builder.dpdk_builder = Some(crate::runtime::scheduler::dpdk::DpdkBuilder::new());
    builder
}
```

### 1.2 問題：無法配置 DPDK 設備

**Root Cause 分析**:

`build_dpdk_runtime()` 在 L1848 讀取 `self.dpdk_builder`，但沒有任何 Builder 方法設置它。

**解決方案**:

在 `Builder` impl 添加配置方法（約 L900 附近，其他配置方法區域）：

```rust
/// Configures a DPDK device for the runtime.
///
/// This method must be called at least once when using `new_dpdk()`.
///
/// # Example
///
/// ```ignore
/// let rt = Builder::new_dpdk()
///     .dpdk_device("0000:00:1f.6")  // PCI address or interface name
///     .build()
///     .unwrap();
/// ```
#[cfg(feature = "rt-multi-thread")]
#[cfg_attr(docsrs, doc(cfg(feature = "rt-multi-thread")))]
pub fn dpdk_device(mut self, device: &str) -> Self {
    if let Some(ref mut dpdk_builder) = self.dpdk_builder {
        *dpdk_builder = std::mem::take(dpdk_builder).device(device);
    }
    self
}

/// Configures multiple DPDK devices for the runtime.
#[cfg(feature = "rt-multi-thread")]
pub fn dpdk_devices(mut self, devices: &[&str]) -> Self {
    if let Some(ref mut dpdk_builder) = self.dpdk_builder {
        *dpdk_builder = std::mem::take(dpdk_builder).devices(devices);
    }
    self
}

/// Adds an EAL argument for DPDK initialization.
#[cfg(feature = "rt-multi-thread")]
pub fn dpdk_eal_arg(mut self, arg: &str) -> Self {
    if let Some(ref mut dpdk_builder) = self.dpdk_builder {
        *dpdk_builder = std::mem::take(dpdk_builder).eal_arg(arg);
    }
    self
}
```

---

## Section 2: TcpDpdkStream 功能完成

### 2.1 問題：TcpDpdkStream::Drop 是空的 TODO

**Root Cause 分析**:

`stream.rs:385-394` 的 `Drop` 實現是空的：
```rust
impl Drop for TcpDpdkStream {
    fn drop(&mut self) {
        // TODO: Remove socket from DpdkDriver's socket set
    }
}
```

**結果**:
- Socket 資源洩漏
- `DpdkDriver::remove_socket()` 和 `unregister_socket()` 是 dead code

**解決方案**:

```rust
impl Drop for TcpDpdkStream {
    fn drop(&mut self) {
        // Remove socket from DpdkDriver via worker context
        let handle = self.handle;
        with_current_driver(|driver| {
            driver.remove_socket(handle);
        });
    }
}
```

### 2.2 問題：無 TcpDpdkListener

**Root Cause 分析**:

標準 TCP 有 `TcpListener::accept()` 返回 `TcpStream`，但 DPDK 模塊沒有 `TcpDpdkListener`。

**結果**:
- `TcpDpdkStream::from_handle()` 是 dead code（它應該被 Listener 調用）
- 無法作為服務器接受連接

**解決方案**:

創建 `listener.rs`（約 200 行）:

```rust
//! TcpDpdkListener implementation.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll};

use smoltcp::iface::SocketHandle;

use super::stream::TcpDpdkStream;
use crate::runtime::scheduler::dpdk::dpdk_driver::ScheduledIo;
use crate::runtime::scheduler::dpdk::{with_current_driver, current_worker_index};

/// A DPDK-backed TCP listener.
pub struct TcpDpdkListener {
    handle: SocketHandle,
    scheduled_io: Arc<ScheduledIo>,
    core_id: usize,
    local_addr: SocketAddr,
}

impl TcpDpdkListener {
    /// Binds to the specified address.
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let core_id = current_worker_index().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "Must be called from DPDK worker")
        })?;

        let (handle, scheduled_io, local_addr) = with_current_driver(|driver| {
            let handle = driver.create_tcp_socket().ok_or_else(|| {
                io::Error::new(io::ErrorKind::OutOfMemory, "No buffers available")
            })?;

            let scheduled_io = driver.register_socket(handle);
            
            // Bind and listen via smoltcp
            let socket = driver.get_tcp_socket_mut(handle);
            let endpoint = match addr {
                SocketAddr::V4(v4) => smoltcp::wire::IpListenEndpoint {
                    addr: Some(smoltcp::wire::IpAddress::Ipv4(
                        smoltcp::wire::Ipv4Address(v4.ip().octets())
                    )),
                    port: v4.port(),
                },
                SocketAddr::V6(_) => return Err(io::Error::new(
                    io::ErrorKind::Unsupported, "IPv6 not supported"
                )),
            };
            
            socket.listen(endpoint).map_err(|e| {
                io::Error::new(io::ErrorKind::AddrInUse, format!("{:?}", e))
            })?;

            Ok::<_, io::Error>((handle, scheduled_io, addr))
        }).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "Cannot access DPDK driver")
        })??;

        Ok(Self { handle, scheduled_io, core_id, local_addr })
    }

    /// Accepts a new connection.
    pub async fn accept(&self) -> io::Result<(TcpDpdkStream, SocketAddr)> {
        // Poll until a connection is accepted
        std::future::poll_fn(|cx| self.poll_accept(cx)).await
    }

    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<(TcpDpdkStream, SocketAddr)>> {
        match self.scheduled_io.poll_read_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(()) => {}
        }

        // Check for new connection via smoltcp
        let result = with_current_driver(|driver| {
            let socket = driver.get_tcp_socket_mut(self.handle);
            
            if socket.is_active() && socket.state() == smoltcp::socket::tcp::State::Established {
                // Connection established - create new stream
                // Note: smoltcp single-socket model requires creating new socket for next conn
                let peer = socket.remote_endpoint().ok_or(io::ErrorKind::NotConnected)?;
                let peer_addr = SocketAddr::new(
                    match peer.addr {
                        smoltcp::wire::IpAddress::Ipv4(v4) => 
                            std::net::IpAddr::V4(std::net::Ipv4Address::from(v4.0)),
                        _ => return Err(io::ErrorKind::Unsupported),
                    },
                    peer.port
                );
                
                Ok(peer_addr)
            } else {
                Err(io::ErrorKind::WouldBlock)
            }
        });

        match result {
            Some(Ok(peer_addr)) => {
                // Create stream from accepted connection
                let stream = TcpDpdkStream::from_handle(
                    self.handle,
                    self.scheduled_io.clone(),
                    self.core_id,
                    Some(self.local_addr),
                    Some(peer_addr),
                );
                Poll::Ready(Ok((stream, peer_addr)))
            }
            Some(Err(io::ErrorKind::WouldBlock)) => {
                self.scheduled_io.clear_read_ready();
                Poll::Pending
            }
            Some(Err(kind)) => Poll::Ready(Err(io::Error::new(kind, "accept failed"))),
            None => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, "driver unavailable"))),
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}
```

---

## Section 3: tokio 功能完整性驗證

### 3.1 問題：DPDK runtime 是否支持所有 tokio 功能？

**分析**:

查看 `build_dpdk_runtime()` (L1843-1920):
- ✅ `blocking_pool` 創建 → `spawn_blocking` 應該工作
- ✅ `driver` 創建 → I/O 和 timer 應該工作
- ✅ `io_thread` 創建 → 標準 I/O (fs, stdin) 應該工作
- ✅ `Handle::Dpdk` → `spawn` 應該工作

**需要建立測試文件驗證**:

創建 `tokio/examples/dpdk_tokio_features_test.rs`:

```rust
//! Verify all tokio features work under DPDK runtime
//!
//! Run: cargo run --example dpdk_tokio_features_test --features rt-multi-thread

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main(flavor = "dpdk")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing tokio features under DPDK runtime...\n");

    // Test 1: spawn
    println!("[1/6] Testing tokio::spawn...");
    let handle = tokio::spawn(async { 42 });
    assert_eq!(handle.await?, 42);
    println!("  ✓ spawn works\n");

    // Test 2: spawn_blocking
    println!("[2/6] Testing tokio::task::spawn_blocking...");
    let result = tokio::task::spawn_blocking(|| {
        std::thread::sleep(Duration::from_millis(10));
        "blocking done"
    }).await?;
    assert_eq!(result, "blocking done");
    println!("  ✓ spawn_blocking works\n");

    // Test 3: sleep
    println!("[3/6] Testing tokio::time::sleep...");
    let start = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(start.elapsed() >= Duration::from_millis(50));
    println!("  ✓ sleep works\n");

    // Test 4: interval
    println!("[4/6] Testing tokio::time::interval...");
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    for _ in 0..3 {
        interval.tick().await;
    }
    println!("  ✓ interval works\n");

    // Test 5: timeout
    println!("[5/6] Testing tokio::time::timeout...");
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        tokio::time::sleep(Duration::from_millis(10))
    ).await;
    assert!(result.is_ok());
    println!("  ✓ timeout works\n");

    // Test 6: channels
    println!("[6/6] Testing tokio::sync::mpsc...");
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    tx.send("hello").await?;
    assert_eq!(rx.recv().await, Some("hello"));
    println!("  ✓ mpsc works\n");

    println!("All tokio features verified successfully!");
    Ok(())
}
```

---

## Section 4: 警告消除（由 Section 1-3 自動解決）

完成 Section 1-3 後，大部分 dead code 警告將自動消失，因為：

1. `Builder::new_dpdk()` 使得 `Kind::Dpdk` 可達
2. `dpdk_device()` 等方法使用 `DpdkBuilder` API
3. `TcpDpdkStream::Drop` 調用 `remove_socket()`
4. `TcpDpdkListener::bind()` 調用 `from_handle()`

**剩餘警告處理**:

| 類型 | 位置 | 解決方案 |
|------|------|----------|
| Hidden lifetime | `dpdk_driver.rs:421,512,518,523` | 改為 `TcpSocket<'_>` |
| Unused import | `worker.rs`, `handle.rs`, `init.rs` | 刪除未使用的 use 語句 |
| Unreachable pub | 全部 dpdk 子模塊 | 改為 `pub(crate)` |

---

## 驗收規格

### 功能驗收清單

- [ ] `Builder::new_dpdk()` 存在並返回 `Kind::Dpdk`
- [ ] `Builder::dpdk_device()` 可配置設備
- [ ] DPDK runtime 可通過 `build()` 創建
- [ ] `TcpDpdkStream::Drop` 調用 `driver.remove_socket()`
- [ ] `TcpDpdkListener` 存在並可 bind/accept
- [ ] tokio features test 全部通過
- [ ] 編譯警告數：0
- [ ] autobahn-testsuite：521/521 PASS

### 編譯驗證

```powershell
cargo check -p tokio --features rt-multi-thread 2>&1 | Out-File cargo_errors.txt
# 必須顯示 "generated 0 warnings"
```

### autobahn-testsuite 驗證（Windows Docker）

```powershell
# 啟動 WebSocket echo server
cargo run --example dpdk_websocket_echo --features rt-multi-thread

# 另一終端運行 autobahn
docker run -it --rm -v ${PWD}/reports:/reports --network host `
    crossbario/autobahn-testsuite wstest -m fuzzingclient
```

---

## 注意事項

1. **Windows DPDK 支持**: Windows 可運行 DPDK（用戶確認）
2. **不使用 `_` 前綴掩蓋警告**: 所有 dead code 必須通過真正使用來解決
3. **不使用 `#[allow(dead_code)]`**: 除非是明確的預留 API 設計
