# DPDK EAL Cleanup 架構重構

## 背景與目的

### 目的
修復 `rte_eal_cleanup()` 調用時產生的 SIGSEGV，正確實現 DPDK 資源生命週期管理。

### 根本原因（已確定）

**問題的本質是 Rust 的 Drop 順序與 DPDK 資源依賴關係衝突。**

```
Runtime {
    scheduler: Scheduler::Dpdk(Dpdk),      // ← 第一個 drop
    handle: Handle { inner: Arc<dpdk::Handle> },  // ← 第二個 drop
    blocking_pool,
}
```

當前問題流程：
```
1. Runtime drop 開始
2. scheduler (Dpdk) drop
   → Dpdk::drop() 調用 rte_eal_cleanup()
   → DPDK EAL 環境被銷毀
3. handle drop
   → Arc<dpdk::Handle> 引用 -1
   → 最後一個引用 drop
      → Shared drop
         → drivers drop
            → DpdkDevice::drop() 嘗試訪問 DPDK 資源
            → SIGSEGV (DPDK 已被銷毀)
```

**Arc<Handle> 被 5 個地方持有**：

| 持有者 | 位置 | 說明 |
|--------|------|------|
| `Dpdk::handle` | mod.rs:65 | 調度器自己 |
| `Worker::handle` | worker.rs:32 | 通過 `Dpdk::worker_0` |
| `IoThread::handle` | io_thread.rs:69 | I/O 線程內部 |
| `IoThreadHandle::scheduler_handle` | io_thread.rs:24 | I/O 線程控制 handle |
| `Runtime::handle.inner` | runtime.rs:103 | **這個在 Dpdk drop 後才釋放！** |

當 `Dpdk::drop()` 執行時，`Arc<Handle>` 還有 2-3 個引用（`Dpdk::handle`、`worker_0.handle`、`Runtime::handle.inner`）。
`rte_eal_cleanup()` 被調用，但之後 `Runtime::handle.inner` drop 觸發 `DpdkDevice::drop()`，訪問已銷毀的 DPDK 資源。

---

## 正確的解決方案

### 核心思路
**將 DPDK 資源（mempool、EAL）的清理延遲到最後一個 `Arc<Handle>` 被 drop 時執行。**

這確保：
1. 所有 `DpdkDevice` 先 drop（安全釋放 mbufs）
2. 然後才釋放 mempool 和調用 EAL cleanup

### 方案：將 mempool 所有權移到 Shared 中

**修改前的所有權**：
```
Dpdk
  ├── handle: Arc<Handle>
  │     └── shared: Shared
  │           └── drivers: DpdkDriver[]
  │                 └── device: DpdkDevice (持有 mempool 指針)
  └── resources: DpdkResourcesHolder
        └── mempool: *mut rte_mempool  ← mempool 所有權在這！
```

**修改後的所有權**：
```
Dpdk
  └── handle: Arc<Handle>
        └── shared: Shared
              ├── drivers: DpdkDriver[]
              │     └── device: DpdkDevice (持有 mempool 指針)
              └── dpdk_resources: DpdkResourcesCleaner  ← 新增！持有 mempool 和 EAL cleanup 責任
```

**關鍵**：`drivers` 在 `dpdk_resources` **之前**聲明，所以 `DpdkDevice` 先 drop，然後才釋放 mempool。

---

## 實現計劃

### 修改 1：創建 `DpdkResourcesCleaner` 結構

**位置**：`tokio/src/runtime/scheduler/dpdk/init.rs`

**內容**：
```rust
/// 持有 DPDK 資源並負責清理的結構。
/// 當 drop 時，釋放 mempool 並調用 rte_eal_cleanup()。
pub(crate) struct DpdkResourcesCleaner {
    mempool: *mut ffi::rte_mempool,
    ports: Vec<u16>,  // 需要 stop/close 的 port IDs
}

impl Drop for DpdkResourcesCleaner {
    fn drop(&mut self) {
        // 1. Stop and close all ports
        for port_id in &self.ports {
            unsafe {
                ffi::rte_eth_dev_stop(*port_id);
                ffi::rte_eth_dev_close(*port_id);
            }
        }
        
        // 2. Free mempool
        if !self.mempool.is_null() {
            unsafe { ffi::rte_mempool_free(self.mempool); }
        }
        
        // 3. Clean up EAL
        unsafe { ffi::rte_eal_cleanup(); }
    }
}
```

### 修改 2：在 Shared 中添加 dpdk_resources 字段

**位置**：`tokio/src/runtime/scheduler/dpdk/worker.rs`

**修改 Shared 結構**：
```rust
pub(crate) struct Shared {
    // ... 現有字段 ...
    
    /// DPDK drivers (one per worker/core) for network I/O polling.
    /// MUST be declared BEFORE dpdk_resources so devices drop before mempool cleanup!
    pub(super) drivers: Box<[std::sync::Mutex<DpdkDriver>]>,
    
    /// DPDK resource cleanup (mempool, EAL). MUST BE LAST FIELD!
    /// This ensures all DpdkDevices are dropped before EAL cleanup.
    pub(super) dpdk_resources: Option<DpdkResourcesCleaner>,
    
    // ... 
}
```

### 修改 3：在 worker::create() 中初始化 dpdk_resources

**位置**：`tokio/src/runtime/scheduler/dpdk/worker.rs`

**修改 `create()` 函數**：接受額外的 `mempool` 和 `ports` 參數，創建 `DpdkResourcesCleaner`。

### 修改 4：修改 Dpdk 結構

**位置**：`tokio/src/runtime/scheduler/dpdk/mod.rs`

**移除 `resources` 字段**：不再需要，mempool 所有權已轉移到 Shared。

### 修改 5：簡化 Dpdk::drop()

**位置**：`tokio/src/runtime/scheduler/dpdk/mod.rs`

**移除自定義 Drop 實現**：不再需要顯式調用 cleanup，讓 Rust 自動 drop。

---

## 正確的關機流程（修復後）

```
Runtime::drop()
  ↓
scheduler (Dpdk) drop
  → Dpdk 字段按順序 drop
      → worker_handles (已空)
      → worker_0 → Arc<Worker> drop → Arc<Handle> -1
      → io_thread_handle (已 None)
      → resource_lock
      → handle: Arc<Handle> drop → 此時還有 Runtime::handle.inner 持有

handle (Runtime::handle) drop
  → inner: scheduler::Handle::Dpdk(Arc<Handle>) drop
      → Arc<Handle> 最後一個引用 drop
          → Handle drop
              → shared: Shared drop
                  → drivers drop (按聲明順序先 drop)
                      → DpdkDriver drop → DpdkDevice::drop()
                          → pktmbuf_free() 釋放 mbufs ← 安全！mempool 還在
                  → dpdk_resources drop (最後)
                      → rte_eth_dev_stop/close
                      → rte_mempool_free() ← 安全！所有 mbufs 已釋放
                      → rte_eal_cleanup() ← 安全！無活躍 DPDK 資源

blocking_pool drop
```

**關鍵**：`dpdk_resources` 在 `drivers` 之後聲明，確保 drop 順序正確。

---

## 需要修改的文件

| 文件 | 修改內容 |
|------|----------|
| `worker.rs` | 添加 `dpdk_resources: Option<DpdkResourcesCleaner>` 到 Shared |
| `init.rs` | 創建 `DpdkResourcesCleaner` 結構，修改返回值 |
| `mod.rs` | 移除 `resources` 字段，移除 `Drop` 實現，修改 `new()` |
| `handle.rs` | 移除 `cleanup_device()` 調用（不再需要） |

---

## 驗收標準

**自動化測試**:
- [x] **AC-1** [test] 所有 `tcp_dpdk_real::*` 測試通過，無 SIGSEGV
- [ ] **AC-2** [test] 所有 `dpdk_worker_isolation::*` 測試通過（測試本身有網路連接問題，但無 SIGSEGV）

**人工驗證**:
- [x] **AC-3** [manual] 測試完成後 `/var/run/dpdk/rte/mp_socket` 不存在

**編譯檢查**:
- [x] **AC-4** [build] cargo check 無錯誤
- [x] **AC-5** [build] cargo check 無新增警告

**代碼品質**:
- [x] **AC-6** [manual] `Dpdk` 結構不再有 `resources` 字段
- [x] **AC-7** [manual] `Dpdk` 不需要自定義 `Drop` 實現
- [x] **AC-8** [manual] `Shared::dpdk_resources` 是結構最後的字段（在 drivers 之後）

---

## 為什麼這是正確的解決方案

1. **利用 Rust 的 Drop 順序**：字段按聲明順序 drop，`drivers` 在 `dpdk_resources` 之前
2. **利用 Arc 語義**：最後一個 `Arc<Handle>` drop 時才觸發 Shared drop
3. **不需要複雜的同步**：完全依賴 Rust 的所有權系統
4. **不需要診斷代碼**：一旦結構正確，不需要任何運行時檢查
