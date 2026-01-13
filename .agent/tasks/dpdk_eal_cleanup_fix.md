# DPDK EAL Cleanup SIGSEGV 修復

## 背景與目的

### 目的
在 DPDK runtime shutdown 時正確調用 `rte_eal_cleanup()` 以清理 `/var/run/dpdk/rte/*` 目錄，同時避免 SIGSEGV。

### 項目上下文
- `tokio-dpdk` 是一個將 DPDK 網路棧整合到 Tokio async runtime 的項目
- DPDK EAL (Environment Abstraction Layer) 初始化後會創建 `/var/run/dpdk/rte/mp_socket` 等文件
- 如果進程結束時沒有調用 `rte_eal_cleanup()`，這些文件會殘留，導致下一次初始化可能失敗
- 目前嘗試在 `DpdkResources::cleanup()` 中調用 `rte_eal_cleanup()` 會導致 **SIGSEGV**

### 問題現象
1. 測試 `test_dpdk_worker_data_isolation` 的**邏輯全部通過**（3 passed, 0 failed）
2. 測試結束後在 Drop/cleanup 階段發生 SIGSEGV
3. 目前 `run_dpdk_tests.sh` 每個測試獨立進程運行，所以進程退出時 OS 會清理資源，問題被掩蓋

### 相關文件
- `tokio/src/runtime/scheduler/dpdk/mod.rs` — `Dpdk` struct 定義和 `shutdown()` 方法
- `tokio/src/runtime/scheduler/dpdk/init.rs` — `DpdkResources` 和 `DpdkResourcesMultiQueue` 定義，包含 `cleanup()` 方法
- `tokio/src/runtime/scheduler/dpdk/worker.rs` — `Worker` struct（持有 `Arc<Handle>`）和 `Shared` struct（持有 `DpdkDriver`）
- `tokio/src/runtime/scheduler/dpdk/dpdk_driver.rs` — `DpdkDriver` struct（持有 `DpdkDevice`）
- `tokio/src/runtime/scheduler/dpdk/device/mod.rs` — `DpdkDevice` struct（持有 mbufs）

---

## 技術決策記錄

### 待調查（需要確認）
- **SIGSEGV 根本原因**: 需要追蹤確認是否是因為 `Arc<Handle>` 引用計數問題導致 `DpdkDevice` 在 `rte_eal_cleanup()` 之後才被 Drop
- **Drop 順序是否正確**: `Dpdk` struct 字段順序已調整為 `resources` 最後，但問題仍然存在

---

## 現狀分析

### 1. Dpdk Struct 字段順序（已調整）

```rust
pub(crate) struct Dpdk {
    worker_handles: Vec<JoinHandle>,       // 1. Drop 第一
    worker_0: Option<Arc<Worker>>,         // 2. Drop 第二
    io_thread_handle: Option<...>,         // 3. Drop 第三
    resource_lock: ResourceLock,           // 4. Drop 第四
    handle: Arc<Handle>,                   // 5. Drop 第五
    resources: Option<DpdkResourcesHolder>,// 6. Drop 第六（最後）
}
```

### 2. 資源持有鏈

```
Dpdk::handle (Arc<Handle>)
  └── Handle::shared (Shared)
        └── Shared::drivers (Box<[Mutex<DpdkDriver>]>)
              └── DpdkDriver::device (DpdkDevice)
                    └── DpdkDevice 持有 mbufs（來自 mempool）

Dpdk::resources (DpdkResourcesHolder)
  └── DpdkResources::mempool (指向 DPDK mempool)
  └── DpdkResources::devices (包含 port 配置)
```

### 3. 問題假設

即使 `resources` 最後 Drop，`Arc<Handle>` 可能還有其他引用：
- `Worker::handle` 持有 `Arc<Handle>`
- `worker::run()` 函數中 `worker.handle.clone()` 創建額外引用

當 `Dpdk` Drop 時：
1. `worker_0: Option<Arc<Worker>>` Drop → `Arc<Worker>` 引用計數 -1
2. **但如果 `Arc<Worker>` 引用計數 > 1**，`Worker` 不會真正 Drop
3. `Worker` 內的 `Arc<Handle>` 仍然存在
4. `handle: Arc<Handle>` Drop → 引用計數 -1，但可能 > 1
5. `resources` Drop → 調用 `rte_eal_cleanup()`
6. **此時 `DpdkDevice`（在 `Shared::drivers` 中）還沒 Drop！**
7. `rte_eal_cleanup()` 釋放了 mempool 底層記憶體
8. 之後當 `DpdkDevice` 真正 Drop 時，嘗試訪問已釋放的 mbuf → **SIGSEGV**

---

## 實現計劃

### 階段 1: 診斷確認

**目的**: 確認 SIGSEGV 的根本原因是 Arc 引用計數問題

**執行過程**:

在 `Dpdk::drop()` 中添加調試輸出，打印：
- `Arc<Handle>` 的 strong_count
- `Arc<Worker>` 的 strong_count（如果 `worker_0` 存在）
- 每個字段 Drop 前後的狀態

運行 `test_dpdk_worker_data_isolation` 測試，觀察輸出確認：
- Drop 時 `Arc<Handle>` 的 strong_count 是否 > 1
- 哪個時間點 strong_count 變為 1

### 階段 2: 識別額外引用來源

**目的**: 找出是誰持有額外的 `Arc<Handle>` 引用

**執行過程**:

檢查 `worker::run()` 函數，追蹤 `Arc<Handle>` 和 `Arc<Worker>` 的 clone 點。

檢查 `Context` struct 是否持有 `Arc<Worker>`（它確實持有：`worker: worker.clone()`）。

確認 worker 線程退出時，這些引用是否被正確釋放。

### 階段 3: 修復方案

根據診斷結果選擇一種方案：

**方案 A: 在 Dpdk::drop() 中手動等待所有引用釋放**

在 drop 函數中使用 spin loop 等待 `Arc<Handle>` 的 strong_count 變為 1，然後再讓 resources Drop。

**方案 B: 將 `rte_eal_cleanup()` 調用移到正確位置**

不在 `DpdkResources::cleanup()` 中調用，而是在確認所有 `DpdkDevice` 已 Drop 後再調用。可能需要重構 Drop 順序或使用 `ManuallyDrop`。

**方案 C: 使用 weak reference 打破循環**

某些持有 `Arc<Handle>` 的地方改用 `Weak<Handle>`，確保 `Dpdk` Drop 時是唯一的 strong reference。

---

## 驗收標準

**自動化測試**:
- [ ] **AC-1** [test] `test_dpdk_worker_data_isolation` 測試通過且無 SIGSEGV
- [ ] **AC-2** [test] 所有 94 個 DPDK 測試通過（使用 `run_dpdk_tests.sh`）
- [ ] **AC-3** [test] 連續運行同一測試兩次，第二次不會因為殘留的 `/var/run/dpdk/rte/*` 文件而失敗

**編譯檢查**:
- [ ] **AC-4** [build] cargo check 無錯誤
- [ ] **AC-5** [build] cargo check 無新增 dead_code 警告

**人工驗證**:
- [ ] **AC-6** [manual] 測試結束後 `/var/run/dpdk/rte/mp_socket` 文件被正確清理

---

## 注意事項

1. **不要使用 workaround**: 如 `rm -rf /var/run/dpdk/rte/*` 或忽略 SIGSEGV 的方法都是掩蓋問題
2. **rte_eal_cleanup() 限制**: DPDK 官方文檔說明：
   - 每個進程只能調用一次
   - 調用後不能再使用任何 DPDK 函數
   - 必須在所有 DPDK 資源（mbufs、mempools）釋放後才能調用
3. **Arc 引用計數**: Rust 的 Drop 順序遵循字段聲明順序，但 `Arc` 只有在 strong_count 為 0 時才會真正 Drop 內部值
4. **現有 dead_code warnings**: `resources` 字段、`cleanup()` 方法顯示為 dead_code，這是因為它們只在 Drop 時被使用，而目前沒有正確調用
