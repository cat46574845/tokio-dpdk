# DPDK Poll 優化方案

## 當前問題

每次事件循環都執行完整的 poll 流程，即使沒有網路活動：

```rust
pub(crate) fn poll(&mut self, now: Instant) -> bool {
    let smol_now = SmolInstant::from_millis(...);  // 時間轉換
    self.device.flush_tx();                         // TX flush
    self.iface.poll(smol_now, ...);                 // 完整協議棧處理
    self.device.flush_tx();                         // 再次 TX flush
    self.dispatch_wakers();                         // 遍歷所有 socket
    result
}
```

開銷估算（256 sockets）：
- `Instant::now()`: ~22 ns
- `flush_tx()` (空): ~5 ns
- `iface.poll()` (無活動): ~200 ns
- `dispatch_wakers()`: ~5 μs (256 × 20 ns)
- **總計: ~5.5 μs per tick**

如果 1M ticks/sec → **5.5 秒 CPU 時間/秒** = 不可行

---

## 優化方案

### 核心思路

1. **有 RX 包** → 完整 poll
2. **有待發送 TX** → 完整 poll  
3. **計時器觸發** → 完整 poll（使用 smoltcp 的 `poll_delay()`）
4. **否則** → 跳過

### 新增字段

```rust
pub(crate) struct DpdkDriver {
    // ... 現有字段 ...
    
    /// 上次完整 poll 的時間
    last_poll_time: Instant,
    
    /// smoltcp 告知的下次 poll 間隔（緩存）
    cached_poll_delay: Option<Duration>,
}
```

### 優化後的 poll

```rust
/// 快速檢查是否需要完整 poll
fn needs_poll(&mut self, now: Instant) -> bool {
    // 1. 檢查是否有 RX 包（調用 rx_burst）
    self.device.try_receive();
    if !self.device.rx_pending_empty() {
        return true;
    }
    
    // 2. 檢查是否有待發送 TX
    if !self.device.tx_buffer_empty() {
        return true;
    }
    
    // 3. 檢查計時器（TCP 重傳、keepalive 等）
    if let Some(delay) = self.cached_poll_delay {
        if now.duration_since(self.last_poll_time) >= delay {
            return true;
        }
    }
    
    false
}

/// 輕量級 poll - 只檢查 RX
pub(crate) fn poll_light(&mut self, now: Instant) -> bool {
    if !self.needs_poll(now) {
        return false;
    }
    
    // 完整 poll
    self.poll_full(now)
}

/// 完整 poll
fn poll_full(&mut self, now: Instant) -> bool {
    let smol_now = self.to_smoltcp_instant(now);
    
    self.device.flush_tx();
    let result = self.iface.poll(smol_now, &mut self.device, &mut self.sockets);
    self.device.flush_tx();
    
    // 更新計時器緩存
    self.last_poll_time = now;
    self.cached_poll_delay = self.iface.poll_delay(smol_now, &self.sockets);
    
    // 只有真正有活動時才 dispatch wakers
    if result {
        self.dispatch_wakers();
    }
    
    result
}
```

---

## 預期效果

### 無流量時

- `needs_poll()`: ~20 ns (rx_burst 返回 0)
- 跳過完整 poll
- **節省 99% CPU**

### 有流量時

- 正常完整 poll
- 額外開銷: 忽略不計

### TCP 計時器

- 通過 `poll_delay()` 精確控制
- 不會錯過重傳/keepalive

---

## 需要的修改

1. `DpdkDevice` 添加 `rx_pending_empty()` 和 `tx_buffer_empty()` 方法
2. `DpdkDriver` 添加 `last_poll_time` 和 `cached_poll_delay` 字段
3. 將 `poll()` 拆分為 `poll_light()` 和 `poll_full()`
4. `worker.rs` 調用 `poll_light()` 替代 `poll()`

---

## 待討論問題

1. **計時器精度**: `poll_delay()` 返回的是「最大可接受延遲」，需要確保不會過度延遲
2. **多 socket 場景**: 只有一個 socket 有活動時，是否可以只 dispatch 那一個？
3. **flush_tx 頻率**: 當前每次都 flush 兩次，可能可以優化

---

## 替代方案

### A. 定期強制 poll

```rust
const FORCE_POLL_INTERVAL_MS: u64 = 1;

fn needs_poll(&self, now: Instant) -> bool {
    has_rx || has_tx || 
    now.duration_since(self.last_poll) > Duration::from_millis(FORCE_POLL_INTERVAL_MS)
}
```

簡單但不精確，可能浪費 CPU 或延遲計時器。

### B. 只優化 dispatch_wakers

```rust
fn dispatch_wakers(&mut self) {
    // 只在有活動時才遍歷
    if self.last_poll_had_activity {
        for (...) { ... }
    }
}
```

改動最小，但只解決部分問題。
