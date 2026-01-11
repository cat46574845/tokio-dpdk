# 多進程多隊列 DPDK 支援

## 背景與目的

### 目的

讓 tokio-dpdk 支援多進程在同一機器上使用不同網卡運行 DPDK，並支援單網卡多隊列（多 worker 共用一張網卡）。

### 項目上下文

當前 tokio-dpdk 的架構是「一設備一 worker」：
- 每個 DPDK 設備只創建 1 個 RX/TX 隊列
- 每個設備對應一個 worker 線程
- 設備和核心是一對一綁定的

新架構需要支援：
- 多進程共享同一機器的 DPDK 資源（不同網卡）
- 單網卡多隊列（一張網卡供多個 worker 使用）
- 運行時資源鎖定（防止多進程衝突）
- 使用 rte_flow 確保流量路由到正確的隊列

### 相關文件

**核心模組**：
- `tokio/src/runtime/scheduler/dpdk/init.rs` — DPDK 初始化
- `tokio/src/runtime/scheduler/dpdk/mod.rs` — 調度器入口
- `tokio/src/runtime/scheduler/dpdk/worker.rs` — Worker 創建
- `tokio/src/runtime/scheduler/dpdk/dpdk_driver.rs` — 網絡驅動
- `tokio/src/runtime/scheduler/dpdk/env_config.rs` — 環境配置解析
- `tokio/src/runtime/scheduler/dpdk/config.rs` — Builder 配置

**配置腳本**：
- `scripts/dpdk/setup.sh` — 主配置腳本
- `scripts/dpdk/platforms/aws-ec2.sh` — AWS 平台配置生成
- `scripts/dpdk/templates/env.example.json` — 配置範例

**FFI 綁定**：
- `tokio/src/runtime/scheduler/dpdk/ffi/bindings_linux.rs` — Linux 綁定
- `tokio/src/runtime/scheduler/dpdk/ffi/dpdk_wrappers.c` — C 包裝函數

---

## 技術決策記錄

### 已確認（用戶決定）

- **檔案鎖機制**: 使用 fs2 crate，檔案鎖在進程死亡時由 OS 自動釋放
- **配置解耦**: 配置腳本只標記可用裝置和核心，不配置具體對應關係
- **IP 分配**: 均分給各隊列（IPv4 和 IPv6 分別均分）
- **流量路由**: 使用 rte_flow 創建顯式規則

### 已決定（代理選擇）

- **鎖文件位置**: `/var/run/dpdk/` — 符合 Linux FHS 標準
- **鎖文件命名**: `device_<pci_address>.lock`, `core_<id>.lock`
- **rte_flow 綁定**: 需要新增到 FFI 綁定（當前缺失）

---

## 實現計劃

### 模組結構

新增模組：

```
tokio/src/runtime/scheduler/dpdk/
├── resource_lock.rs    # 新增：資源鎖定管理
├── allocation.rs       # 新增：裝置/核心分配邏輯
└── flow_rules.rs       # 新增：rte_flow 規則創建
```

---

### 結構體: `ResourceLock`

**目的**: 管理 DPDK 資源（設備和核心）的運行時鎖定

```rust
pub(crate) struct ResourceLock {
    device_locks: Vec<(String, File)>,  // (pci_address, lock_file)
    core_locks: Vec<(usize, File)>,     // (core_id, lock_file)
}
```

**相關常量**:
- `LOCK_DIR`: `/var/run/dpdk/`

---

### 函數: `ResourceLock::acquire_devices`

**目的**: 嘗試鎖定指定的 DPDK 設備

**簽名**: `fn acquire_devices(pci_addresses: &[String]) -> io::Result<Vec<(String, File)>>`

**執行過程**:

首先確保 `LOCK_DIR` 存在，如果不存在則創建（權限 0755）。

對每個 pci_address：
1. 構造鎖檔路徑：`{LOCK_DIR}/device_{pci_address}.lock`（將 `:` 替換為 `_`）
2. 打開或創建檔案（OpenOptions::create(true).write(true)）
3. 調用 fs2::FileExt::try_lock_exclusive()
4. 如果成功，添加到結果列表
5. 如果失敗（WouldBlock），返回錯誤，說明哪個設備被占用

---

### 函數: `ResourceLock::acquire_cores`

**目的**: 嘗試鎖定指定的 CPU 核心

**簽名**: `fn acquire_cores(core_ids: &[usize]) -> io::Result<Vec<(usize, File)>>`

**執行過程**:

與 acquire_devices 類似，鎖檔路徑為 `{LOCK_DIR}/core_{id}.lock`。

---

### 函數: `ResourceLock::release`

**目的**: 釋放所有持有的鎖

**簽名**: `fn release(&mut self)`

**執行過程**:

對每個持有的 File 調用 fs2::FileExt::unlock()。
清空 device_locks 和 core_locks。

注意：即使不顯式調用，進程終止時 OS 會自動釋放。

---

### 結構體: `AllocationPlan`

**目的**: 描述設備、核心、IP 的分配計劃

```rust
pub(crate) struct AllocationPlan {
    /// 每個 worker 的配置
    pub workers: Vec<WorkerAllocation>,
}

pub(crate) struct WorkerAllocation {
    /// DPDK 設備 PCI 地址
    pub pci_address: String,
    /// 隊列 ID（同一設備的不同 worker 使用不同隊列）
    pub queue_id: u16,
    /// 綁定的 CPU 核心
    pub core_id: usize,
    /// 分配的 IPv4 地址
    pub ipv4: Option<IpCidr>,
    /// 分配的 IPv6 地址
    pub ipv6: Option<IpCidr>,
    /// MAC 地址
    pub mac: [u8; 6],
    /// IPv4 閘道
    pub gateway_v4: Option<Ipv4Address>,
    /// IPv6 閘道
    pub gateway_v6: Option<Ipv6Address>,
}
```

---

### 函數: `create_allocation_plan`

**目的**: 根據可用資源和用戶請求創建分配計劃

**簽名**: 
```rust
fn create_allocation_plan(
    env_config: &DpdkEnvConfig,
    requested_devices: Option<&[String]>,
    requested_num_cores: Option<usize>,
    resource_lock: &ResourceLock,
) -> Result<AllocationPlan, AllocationError>
```

**執行過程**:

首先確定可用設備：
1. 獲取 env_config 中所有 role=dpdk 的設備
2. 過濾掉已被其他進程鎖定的設備
3. 如果 requested_devices 有指定，只保留指定的設備
4. 按字典序排序

然後確定可用核心：
1. 從 env_config 獲取配置的核心範圍（TODO：需要擴展 env.json 格式）
2. 過濾掉已被鎖定的核心
3. 按數字順序排序

接著進行匹配：
- 設 D = 可用設備數量，C = 可用核心數量
- 如果 C < D：選擇前 C 個設備（按字典序），每設備 1 隊列，每隊列 1 核心
- 如果 C >= D：按字典序循環分配設備給核心

IP 均分邏輯：
- 對於每個設備，按分配到的核心數量均分其 IPv4 和 IPv6 地址
- 如果某設備分配了 N 個核心但只有 M 個 IPv4（M < N），前 M 個 worker 各得 1 個，後面的沒有
- 如果某設備沒有任何 IP（IPv4 和 IPv6 都沒有足夠），返回錯誤

驗證：
- 每個 worker 至少有 1 個 IP（v4 或 v6）
- 如果驗證失敗，返回 AllocationError::InsufficientIps

---

### 函數: `init_port_multi_queue`

**目的**: 初始化單個 DPDK port 支援多隊列

**簽名**: 
```rust
fn init_port_multi_queue(
    port_id: u16,
    mempool: *mut ffi::rte_mempool,
    num_queues: u16,
) -> io::Result<()>
```

**執行過程**:

與現有 init_port 類似，但：
1. 調用 rte_eth_dev_configure 時傳入 num_queues（而非固定的 1）
2. 循環創建 num_queues 個 RX 和 TX 隊列
3. 如果 num_queues > 1，配置 RSS 模式（但實際路由靠 rte_flow）

---

### 函數: `create_flow_rules`

**目的**: 為每個 IP 創建 rte_flow 規則，確保流量送到正確隊列

**簽名**: 
```rust
unsafe fn create_flow_rules(
    port_id: u16,
    allocations: &[(IpCidr, u16)],  // (ip, queue_id)
) -> io::Result<Vec<*mut rte_flow>>
```

**執行過程**:

對每個 (ip, queue_id) 對：

1. 構建 pattern：
   - RTE_FLOW_ITEM_TYPE_ETH
   - RTE_FLOW_ITEM_TYPE_IPV4（如果是 v4）或 IPV6
   - 設置 hdr.dst_addr 為目標 IP
   - RTE_FLOW_ITEM_TYPE_END

2. 構建 action：
   - RTE_FLOW_ACTION_TYPE_QUEUE，queue.index = queue_id
   - RTE_FLOW_ACTION_TYPE_END

3. 構建 attr：
   - ingress = 1
   - priority = 0

4. 調用 rte_flow_create(port_id, &attr, pattern, actions, &error)
5. 如果失敗，記錄錯誤並返回

注意：需要先新增 rte_flow 相關的 FFI 綁定。

---

### FFI 綁定擴展

**目的**: 新增 rte_flow API 的 Rust 綁定

需要新增的函數：
- `rte_flow_create`
- `rte_flow_destroy`
- `rte_flow_validate`

需要新增的類型：
- `rte_flow`
- `rte_flow_attr`
- `rte_flow_item`
- `rte_flow_item_ipv4`
- `rte_flow_item_ipv6`
- `rte_flow_action`
- `rte_flow_action_queue`
- `rte_flow_error`

**實現方式**：
1. 在 `tools/generate_dpdk_bindings.sh` 中添加 `rte_flow.h` 頭文件
2. 重新生成 bindings

或者手動在 dpdk_wrappers.c 中創建包裝函數。

---

### 配置腳本修改

**目的**: 修改 setup.sh 和 aws-ec2.sh 支援新格式

#### env.json 格式擴展

新增頂層字段：

```json
{
  "version": 2,
  "dpdk_cores": [1, 2, 3, 4],  // 新增：可用於 DPDK 的核心列表
  "devices": [
    {
      "pci_address": "0000:28:00.0",
      "role": "dpdk",
      // ... 其他字段不變
    }
  ]
}
```

移除 `core_affinity` 字段。

#### setup.sh wizard 修改

在 CPU 分配步驟：
- 不再為每個設備指定核心
- 只詢問「哪些核心可用於 DPDK」

在 NIC 分配步驟：
- 只詢問「哪些網卡可用於 DPDK」
- 不詢問對應關係

---

### Builder API 修改

**目的**: 允許用戶指定設備和核心數量

新增方法：

```rust
impl Builder {
    /// 指定要使用的 DPDK 設備（PCI 地址列表）
    /// 如果不調用，使用所有可用設備
    pub fn dpdk_devices(&mut self, devices: &[&str]) -> &mut Self

    /// 指定要使用的核心數量
    /// 如果不調用，使用所有可用核心
    pub fn dpdk_num_workers(&mut self, count: usize) -> &mut Self
}
```

---

### mod.rs 修改

**目的**: 整合新的分配邏輯

修改 `Dpdk::new()`:

1. 加載 env_config
2. 創建 ResourceLock
3. 調用 create_allocation_plan
4. 鎖定分配的設備和核心
5. 對每個唯一的 port_id 調用 init_port_multi_queue
6. 對每個 port_id 調用 create_flow_rules
7. 創建 workers（根據 AllocationPlan）

---

### worker.rs 修改

**目的**: 支援多隊列模式

修改 `create()` 函數簽名：

```rust
pub(super) fn create(
    allocations: Vec<WorkerAllocation>,
    mempool: *mut ffi::rte_mempool,
    driver_handle: driver::Handle,
    blocking_spawner: blocking::Spawner,
    seed_generator: RngSeedGenerator,
    config: Config,
) -> (Arc<Handle>, Launch)
```

在創建 DpdkDevice 時傳入 queue_id（從 WorkerAllocation 獲取）。

---

## 驗收標準

### 一、新功能單元測試

#### 1.1 ResourceLock 測試

- [x] [test] `test_resource_lock_acquire_device_success` — 成功鎖定單個設備
- [x] [test] `test_resource_lock_acquire_multiple_devices` — 成功鎖定多個設備
- [x] [test] `test_resource_lock_acquire_core_success` — 成功鎖定單個核心
- [x] [test] `test_resource_lock_acquire_multiple_cores` — 成功鎖定多個核心
- [x] [test] `test_resource_lock_device_already_locked` — 設備已被鎖定時返回 WouldBlock 錯誤
- [x] [test] `test_resource_lock_core_already_locked` — 核心已被鎖定時返回 WouldBlock 錯誤
- [x] [test] `test_resource_lock_release` — 釋放後其他進程可獲取
- [x] [test] `test_resource_lock_drop_releases` — Drop 時自動釋放鎖
- [x] [test] `test_resource_lock_dir_creation` — 鎖目錄不存在時自動創建

#### 1.2 AllocationPlan 測試

- [x] [test] `test_allocation_single_device_single_core` — 1 設備 1 核心，正確分配
- [x] [test] `test_allocation_single_device_multi_core` — 1 設備 3 核心，均分 3 個 IPv4
- [x] [test] `test_allocation_single_device_multi_core_uneven_ip` — 1 設備 3 核心但只有 2 個 IPv4，前 2 個 worker 各得 1 個
- [x] [test] `test_allocation_multi_device_less_cores` — 3 設備 2 核心，選擇前 2 個設備（字典序）
- [x] [test] `test_allocation_multi_device_more_cores` — 2 設備 4 核心，循環分配（設備 0 得 2 核，設備 1 得 2 核）
- [x] [test] `test_allocation_insufficient_ip` — 設備無 IP 時返回 InsufficientIps 錯誤 (實際名稱: `test_allocation_no_ip_returns_error`)
- [x] [test] `test_allocation_no_available_devices` — 無可用設備時返回錯誤
- [x] [test] `test_allocation_no_available_cores` — 無可用核心時返回錯誤
- [x] [test] `test_allocation_device_sort_order` — 驗證設備按 PCI 地址字典序排序
- [x] [test] `test_allocation_ipv6_distribution` — 驗證 IPv6 正確均分

#### 1.3 FlowRules 測試

- [x] [test] `test_flow_rule_ipv4_pattern` — 生成的 IPv4 pattern 結構正確 (實際名稱: `test_flow_pattern_builder_ipv4`)
- [x] [test] `test_flow_rule_ipv6_pattern` — 生成的 IPv6 pattern 結構正確 (實際名稱: `test_flow_pattern_builder_ipv6`)
- [x] [test] `test_flow_rule_queue_action` — 生成的 queue action 指向正確隊列
- [x] [test] `test_flow_rule_multiple_ips` — 多 IP 時創建多條規則

#### 1.4 InitPortMultiQueue 測試

- [x] [test] `test_init_port_single_queue` — 單隊列初始化（向後兼容）(在 `test_dpdk_multi_queue_all` 中)
- [x] [test] `test_init_port_multi_queue` — 多隊列初始化成功 (在 `test_dpdk_multi_queue_all` 中)

### 二、整合測試

#### 2.1 Builder API 測試

- [x] [test] `test_builder_dpdk_devices_filter` — 指定設備列表正確過濾
- [x] [test] `test_builder_dpdk_num_workers` — 指定核心數量正確限制 (實際名稱: `test_dpdk_builder_with_num_workers`)
- [x] [test] `test_builder_no_dpdk_devices_uses_all` — 不指定設備時使用所有可用
- [x] [test] `test_builder_no_num_workers_uses_all` — 不指定核心數時使用所有可用 (實際名稱: `test_dpdk_builder_no_num_workers_uses_default`)
- [x] [test] `test_builder_invalid_device_error` — 指定不存在設備時返回錯誤
- [x] [test] `test_builder_zero_workers_error` — 指定 0 核心時返回錯誤 (實際名稱: `test_dpdk_builder_zero_workers_error`)

#### 2.2 多進程測試

測試在 `dpdk_multi_process.rs` 中實現：

- [x] [test] `test_dpdk_multi_queue_all::subtest_different_device_lock_available` — 兩個進程使用不同設備正常運行
- [x] [test] `test_dpdk_multi_queue_all::subtest_same_device_lock_held` — 兩個進程嘗試使用同一設備，後者失敗
- [x] [test] `test_multi_process_lock_release_on_exit` — 進程正常退出後設備可被重新獲取

### 三、現有 DPDK 測試迴歸

執行 `./run_dpdk_tests.sh` 確保所有現有測試通過：

- [x] [test] `test_runtime_creation` — Runtime 創建成功 (由 `dpdk_specific::*` 測試涵蓋)
- [x] [test] `test_spawn_basic` — 基本任務 spawn (由 `dpdk_specific::dpdk_runtime_spawn` 涵蓋)
- [x] [test] `test_tcp_dpdk_connect` — TCP 連接測試 (由 `api_parity::tcp_dpdk_stream_connect` 涵蓋)
- [x] [test] `test_tcp_dpdk_echo` — TCP echo 測試 (由 `api_parity::tcp_stream_read_write` 涵蓋)
- [x] [test] `test_tcp_dpdk_concurrent` — 並發連接測試 (由 `buffer_pool_tests::concurrent_connections_stress` 涵蓋)
- [x] [test] `test_dpdk_driver_poll` — Driver poll 測試 (由 `dpdk_scheduler::io_driver_called_when_under_load` 涵蓋)
- [x] [test] `test_dpdk_device_rx_tx` — 設備收發測試 (由 `test_dpdk_network_all` 涵蓋)
- [x] [test] `test_worker_affinity` — Worker 親和性測試 (由 `cpu_affinity_tests::*` 測試涵蓋)
- [x] [test] 其他所有 `tokio/tests/` 下的 DPDK 相關測試 — **97 個測試全部通過**

**執行命令**:
```bash
cd /home/ubuntu/tokio-dpdk
sudo ./run_dpdk_tests.sh
```

**結果**: ✅ 97 passed, 0 failed, 0 skipped

### 四、端對端整合測試

所有 E2E 測試在 `test_dpdk_multi_queue_all` 中作為子測試執行：

#### 4.1 多進程流量測試

- [x] [test] `subtest_real_network_http` — 真實 HTTP 流量測試
- [x] [test] `subtest_multiple_connections` — 多連接並發測試（10 連接）

#### 4.2 多隊列流量路由測試

- [x] [test] `subtest_concurrent_workers_traffic` — 多 worker 並發處理流量
- [x] [test] `subtest_traffic_distribution` — TX/RX 流量分佈驗證

### 五、人工驗證

- [manual] 進程崩潰（kill -9）後，另一個進程可以立即獲取該設備的鎖
- [manual] 使用 `dpdk-devbind.py --status` 確認設備綁定狀態正確

### 六、編譯檢查

- [x] [build] `cargo check -p tokio --features full` 無錯誤
- [x] [build] `cargo check -p tokio --features full` 無新增警告
- [x] [build] `cargo clippy -p tokio --features full` 無新增警告 (僅有 build.rs 預先存在的警告)
- [x] [build] `cargo test -p tokio --no-run --features full` 編譯成功

### 七、文檔更新

所有受影響的文檔必須更新：

#### 6.1 用戶文檔

- [x] [doc] `TOKIO_DPDK_GUIDE.md` — 新增多進程使用說明章節
  - ✅ 說明如何配置多進程環境
  - ✅ 說明 Builder API 的新方法（dpdk_devices, dpdk_num_workers）
  - ✅ 說明 IP 均分機制
  - ✅ 說明資源鎖定行為

#### 6.2 配置腳本文檔

- [x] [doc] `scripts/dpdk/README.md` — 更新配置流程說明
  - ✅ 說明新的配置模式（只標記可用資源，不配置對應關係）
  - ✅ 說明 dpdk_cores 字段
  - ✅ 說明 env.json version 2 格式

#### 6.3 範例配置

- [x] [doc] `scripts/dpdk/templates/env.example.json` — 更新為 version 2 格式
  - ✅ 新增 dpdk_cores 範例
  - ✅ 移除 core_affinity

#### 7.4 API 文檔

- [x] [doc] 所有新增公開 API 必須有完整 rustdoc 註釋
  - ✅ `Builder::dpdk_devices()` — 在 config.rs 中有完整文檔
  - ✅ `Builder::dpdk_num_workers()` — 在 config.rs 中有完整文檔
  - ✅ 相關錯誤類型 — `AllocationError`, `ConfigError` 均有文檔

### 八、向後兼容性

- [x] [compat] 不調用新 Builder 方法時，行為與修改前一致
- [x] [compat] 單設備單核心配置無需任何改動

---

## 注意事項

1. **rte_flow 綁定缺失**：當前 FFI 綁定不包含 rte_flow API，需要先擴展綁定
2. **AWS ENA 支援**：確認 ENA 驅動支援 rte_flow（已知有限支援）
3. **資源清理**：確保 shutdown 時釋放鎖和銷毀 flow rules

