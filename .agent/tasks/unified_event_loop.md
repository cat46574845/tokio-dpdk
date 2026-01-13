# Unified Event Loop Architecture

## 背景與目的

### 目的
修復 DPDK 事件循環架構缺陷：`run_with_future` 缺少 `maybe_maintenance()` 調用，導致 buffer pool 無法補充。

### 項目上下文
當前 `block_on` 使用獨立的事件循環（`run_with_future`），與 worker 線程的 `Context::run()` 不同。這導致 `replenish_buffer_pool_if_needed` 從未執行，累積測試後 buffer 耗盡，DPDK 連接出現 "0 ops"。

### 相關文件
- `tokio/src/runtime/scheduler/dpdk/worker.rs` — Worker、Core、Context 定義
- `tokio/src/runtime/scheduler/dpdk/mod.rs` — Dpdk::block_on 入口

---

## 技術決策記錄

### 已確認（用戶決定）
- **同步機制**: 使用 Condvar + Mutex（非 spin-wait）
- **Core 傳輸**: 使用 SendWrapper 進行 unsafe impl Send
- **線程模型**: Worker 0 收到 transfer 信號後直接退出，下次 block_on 時 spawn 新線程
- **CPU 親和性**: block_on 內 main 線程在 worker 0 核心上；block_on 外 main 線程不在任何 worker 核心上
- **事件循環**: 只有一個 `Context::run()`，在 loop 中加入 poll block_on future 的步驟

---

## 實現計劃

### 結構體 1: `SendWrapper<T>`

**目的**: 將 !Send 類型包裝為 Send，用於通過 Condvar 傳輸 Core

**定義**:
```rust
pub(crate) struct SendWrapper<T>(T);
unsafe impl<T> Send for SendWrapper<T> {}

impl<T> SendWrapper<T> {
    pub unsafe fn new(val: T) -> Self { SendWrapper(val) }
    pub fn into_inner(self) -> T { self.0 }
}
```

---

### 結構體 2: `TransferSync`

**目的**: 同步 worker 0 與 main 線程的 Core 傳輸

**定義**:
```rust
pub(crate) struct TransferSync {
    pub mutex: Mutex<Option<SendWrapper<Box<Core>>>>,
    pub condvar: Condvar,
}
```

---

### 修改 1: Context 新增 block_on_future 欄位

**目的**: 讓 `Context::run()` 知道是否需要 poll block_on future

**定義**:
```rust
pub(crate) struct Context {
    pub(crate) worker: Arc<Worker>,
    pub(crate) core: RefCell<Option<Box<Core>>>,
    pub(crate) defer: Defer,
    // 新增：block_on future 相關狀態
    pub(crate) block_on_state: RefCell<Option<BlockOnState>>,
}

struct BlockOnState {
    // 使用 type erasure 存儲 future 和結果
    poll_fn: Box<dyn FnMut(&mut std::task::Context<'_>) -> Poll<()>>,
    completed: bool,
}
```

---

### 修改 2: Context::run() 新增步驟

**位置**: `worker.rs:L627-666`

**修改後的 loop**:

```
loop {
    // 1. 檢查 shutdown
    if self.worker.handle.is_shutdown() { ... }
    
    // 2. tick
    self.tick();
    
    // 3. poll DPDK driver
    self.poll_dpdk_driver();
    
    // 4. 執行 tasks
    if let Some(task) = self.next_task_with_fairness(...) { ... }
    
    // 5. 【新增】poll block_on future（如果有）
    if let Some(state) = self.block_on_state.borrow_mut().as_mut() {
        if !state.completed {
            let waker = ...; // 創建本地 waker
            let mut cx = std::task::Context::from_waker(&waker);
            if (state.poll_fn)(&mut cx).is_ready() {
                state.completed = true;
                // 設置 shutdown 以退出 loop
                if let Some(core) = self.core.borrow_mut().as_mut() {
                    core.is_shutdown = true;
                }
            }
        }
    }
    
    // 6. maintenance（包含 buffer replenish）
    self.maybe_maintenance();
    
    // 7. defer wake
    self.defer.wake();
}
```

---

### 函數 1: `transfer_core`

**目的**: 保存 Core 並退出線程

**執行過程**:

從 `self.core` 取出 Core。

使用 `SendWrapper::new` 包裝。

取得 `TransferSync` 的 lock，放入 Core。

呼叫 `condvar.notify_one()`。

返回 `Err(())`，線程直接退出。

---

### 函數 2: `run_with_future`

**目的**: main 線程接管 worker 0 並運行統一事件循環

**執行過程**:

**進入階段**:

呼叫 `pthread_getaffinity_np` 保存 main 線程當前的 cpuset。

設置 `transfer_mode = true` (Release)，設置 `shutdown = true`。

取得 `TransferSync` 的 lock，`condvar.wait_while(|opt| opt.is_none())`。

取出 Core。

呼叫 `pthread_setaffinity_np` 將 main 線程 pin 到 worker 0 的核心。

重置 `shutdown = false`。

**運行階段**:

創建 Context，設置 `block_on_state` 為包裝後的 future。

呼叫 `Context::run(core)` — **與 worker 線程使用相同的函數**。

**退出階段**:

將 Core 放回 AtomicCell。

設置 `transfer_mode = false`。

Spawn 新的 worker 0 線程。

呼叫 `pthread_setaffinity_np` 恢復 main 線程的 cpuset。

從 `block_on_state` 提取結果並返回。

---

### API: `spawn_on_core` / `spawn_local_core`

```rust
pub fn spawn_on_core<F>(core_id: usize, future: F) -> JoinHandle<F::Output>
pub fn spawn_local_core<F>(future: F) -> JoinHandle<F::Output>
```

---

## 驗收標準

### 函數: `Context::run`

**正常路徑**:
- [x] **AC-1** [test] `test_run_executes_spawned_tasks` — spawn 10 個 tasks → 全部被執行

**block_on future polling**:
- [x] **AC-2** [test] `test_run_polls_block_on_future` — 設置 block_on_state 後呼叫 run → future 被 poll 至完成
- [x] **AC-3** [test] `test_run_exits_on_block_on_complete` — block_on future Ready → loop 退出

**maintenance**:
- [x] **AC-4** [test] `test_maybe_maintenance_called` — 運行 1000 ticks → maintenance 被調用
- [x] **AC-5** [test] `test_buffer_replenish_called` — buffer 低於 watermark → replenish 被調用

---

### 函數: `run_with_future`

**CPU 親和性**:
- [x] **AC-6** [test] `test_cpu_affinity_during_block_on` — block_on 內 `sched_getcpu()` → worker 0 核心
- [x] **AC-7** [test] `test_cpu_affinity_outside_block_on` — block_on 前後 `sched_getcpu()` → 相同的非 worker 核心

**狀態轉換**:
- [x] **AC-8** [test] `test_worker0_state_transfer` — block_on 開始 → worker 0 退出、Core 進入 TransferSync
- [x] **AC-9** [test] `test_worker0_recovery` — block_on 結束 → 新 worker 0 被 spawn

**冪等性**:
- [x] **AC-10** [test] `test_multiple_block_on` — 連續 3 次 block_on → 每次都正確執行

---

### 整體架構

**單一事件循環**:
- [x] **AC-11** [test] `test_single_run_function` — Context::run 被 worker 和 main 共用

**Worker 運行狀態**:
- [x] **AC-12** [test] `test_all_workers_running_after_build` — build 後所有 worker 都在 run()
- [x] **AC-13** [test] `test_spawn_before_block_on` — build 後 spawn tasks → 被執行

---

### 新增 API

- [x] **AC-14** [test] `test_spawn_on_core` — spawn_on_core(1, task) → task 在 worker 1 執行
- [x] **AC-15** [test] `test_spawn_local_core` — 在 worker 2 呼叫 → task 在 worker 2 執行

---

### 編譯檢查
- [x] **AC-16** [build] `cargo check --package tokio --features full` → 無錯誤 無新警告

---

## 注意事項

1. **單一事件循環**: 不要創建 `run_with_block_on`，在 `Context::run` 中加入步驟
2. **Type erasure**: 使用 `Box<dyn FnMut>` 避免泛型污染 Context 結構
3. **Waker**: block_on future 的 waker 可以簡單地做 nothing（busy poll）
4. **調試日誌**: 完成後移除所有 `[DEBUG]` 日誌
