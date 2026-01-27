# DPDK + Low Latency Environment Setup

用於配置 tokio-dpdk 運行環境和系統級低延遲優化的統一腳本。

> 這是 [tokio-dpdk](../../TOKIO_DPDK_GUIDE.md) 的環境配置工具。

## 設計目標

確保 DPDK 核心**絕對不會**被以下任何來源打擾：

| 干擾來源 | 解決方案 |
|----------|----------|
| 進程調度 | `isolcpus` - 從調度器隔離 DPDK 核心 |
| 內核定時器 | `nohz_full` - 消除 timer tick |
| RCU 回調 | `rcu_nocbs` - 將 RCU 移出 DPDK 核心 |
| 管理網卡 IRQ | IRQ 綁定到非 DPDK 核心 |
| 其他設備 IRQ | `irqaffinity` + 顯式綁定 |
| 記憶體換頁 | `vm.swappiness=0` + 禁用 THP |

## 快速開始（新機器一站式配置）

```bash
# 以 root 運行互動式 wizard
sudo ./setup.sh wizard
```

Wizard 會引導您完成：
1. 硬件檢測（CPU、網卡）
2. CPU 核心分配（DPDK vs 系統）
3. 網卡分配（DPDK vs 管理）
4. Hugepages 配置
5. DPDK 安裝
6. 配置持久化（GRUB、systemd）
7. 網卡綁定

## 命令參考

### 初始設置

| 命令 | 說明 |
|------|------|
| `wizard` | 互動式一站配置（新機器推薦）|
| `detect` | 檢測硬件並顯示建議 |

### 安裝

| 命令 | 說明 |
|------|------|
| `dpdk-install` | 下載並編譯安裝 DPDK（詳細進度輸出）|
| `dpdk-bind` | 綁定網卡到 DPDK（vfio-pci）|

### 配置管理

| 命令 | 說明 |
|------|------|
| `apply` | 應用運行時配置（不重啟）|
| `persist` | 持久化配置（GRUB + systemd）|
| `refresh-config` | **刷新 IP 配置**（從 AWS Metadata 重新獲取）|

### 狀態

| 命令 | 說明 |
|------|------|
| `verify` | 驗證配置狀態 |

## IP 配置刷新

當 AWS EC2 的 IP 發生變化時（例如添加/刪除 Elastic IP 或 Secondary IP），執行：

```bash
sudo ./setup.sh refresh-config
```

這會從 AWS Metadata Service 重新獲取所有網卡的 IP 配置，更新 `/etc/dpdk/env.json`。

## 配置架構

```
┌─────────────────────────────────────────────────────────────┐
│                        系統總覽                              │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│   ┌───────────────────┐     ┌───────────────────┐          │
│   │   DPDK 核心        │     │   系統核心         │          │
│   │   (isolcpus)      │     │   (irqaffinity)   │          │
│   │                   │     │                   │          │
│   │  • CPU 1, 2, 3    │     │  • CPU 0          │          │
│   │  • 無 IRQ         │     │  • 處理所有 IRQ    │          │
│   │  • 無 timer tick  │     │  • 運行系統進程    │          │
│   │  • DPDK 輪詢      │     │                   │          │
│   └───────────────────┘     └───────────────────┘          │
│                                                             │
│   ┌───────────────────┐     ┌───────────────────┐          │
│   │   DPDK 網卡        │     │   管理網卡         │          │
│   │   (vfio-pci)      │     │   (kernel driver) │          │
│   │                   │     │                   │          │
│   │  • enp40s0        │     │  • enp39s0        │          │
│   │  • enp41s0        │     │  • SSH/管理連接    │          │
│   │  • 用戶態輪詢      │     │  • IRQ → 系統核心  │          │
│   └───────────────────┘     └───────────────────┘          │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

## 配置文件

| 文件 | 說明 |
|------|------|
| `/etc/dpdk/low-latency.conf` | 主配置（CPU、NIC 分配）|
| `/etc/dpdk/env.json` | DPDK 環境配置（供 tokio-dpdk 讀取）|
| `/etc/sysctl.d/99-dpdk-low-latency.conf` | sysctl 參數 |
| `/etc/default/grub` | GRUB 內核參數 |
| `/etc/systemd/system/dpdk-low-latency.service` | 開機自動配置 |

### env.json 格式

```json
{
  "dpdk_cores": [1, 2, 3, 4],
  "devices": [
    {
      "pci_address": "0000:28:00.0",
      "role": "dpdk",
      ...
    }
  ]
}
```

**關鍵字段**：

| 字段 | 說明 |
|------|------|
| `dpdk_cores` | **必填** — 可供 DPDK 使用的 CPU 核心列表 |
| `devices[].role` | `dpdk` 或 `kernel`，標識設備用途 |

**運行時忽略的字段**（用於人類閱讀或腳本內部使用）：

| 字段 | 說明 |
|------|------|
| `version` | Schema 版本，腳本生成 |
| `platform` | 平台標識 |
| `generated_at` | 生成時間戳 |
| `eal_args` | 透傳至 `rte_eal_init()`，與 Builder API 的 `dpdk_eal_args()` 合併 |

**注意**：version 2 格式下，配置腳本只標記可用資源（設備和核心），不配置具體的設備-核心對應關係。運行時會根據 `dpdk_pci_addresses()` 和 `worker_threads()` API 自動分配。

## 內核參數詳解

### GRUB 參數（需要重啟）

```
GRUB_CMDLINE_LINUX="isolcpus=1,2,3 nohz_full=1,2,3 rcu_nocbs=1,2,3 rcu_nocb_poll irqaffinity=0 default_hugepagesz=2M hugepagesz=2M hugepages=512"
```

| 參數 | 說明 | 來源 |
|------|------|------|
| `isolcpus` | 從調度器隔離 CPU | [DPDK 官方文檔](https://doc.dpdk.org/guides/linux_gsg/nic_perf_intel_platform.html) |
| `nohz_full` | 禁用 timer tick | [內核文檔](https://www.kernel.org/doc/Documentation/timers/NO_HZ.txt) |
| `rcu_nocbs` | 將 RCU 回調移出 | [DPDK 調優指南](https://doc.dpdk.org/) |
| `irqaffinity` | 默認 IRQ 親和 | [DPDK 最佳實踐](https://doc.dpdk.org/) |
| `hugepages` | 預留大頁內存 | [DPDK 文檔](https://doc.dpdk.org/guides/linux_gsg/sys_reqs.html) |

### sysctl 參數（運行時）

```
vm.swappiness = 0           # 避免換頁
kernel.numa_balancing = 0   # 禁用自動 NUMA 遷移
```

### 運行時設置

- 停止 `irqbalance`
- 禁用 Transparent Huge Pages (THP)
- CPU governor 設為 `performance`
- 所有設備 IRQ 綁定到系統核心

## AWS EC2 特別說明

在 AWS EC2 上：

1. **主網卡（device 0）保留給 SSH** — 絕對不要綁定到 DPDK
2. **使用 vfio-pci no-IOMMU 模式** — EC2 不支持標準 IOMMU
3. **ENA MTU 9001** — 支持 Jumbo Frame
4. **IP 從 Metadata Service 獲取** — 網卡綁定後無法訪問 `/sys/class/net`

## 低延遲優化總覽

此腳本整合了所有 DPDK 低延遲所需的系統配置：

| 功能 | 說明 |
|------|------|
| CPU 隔離 | `isolcpus`, `nohz_full`, `rcu_nocbs` |
| IRQ 綁定 | 確保 IRQ 不干擾 DPDK 核心 |
| irqbalance 停止 | 需要靜態 IRQ 配置 |
| THP 禁用 | 避免延遲抖動 |
| vm.swappiness=0 | 避免記憶體換頁 |
| CPU governor | 設為 performance 模式 |

## 故障排除

### DPDK 測試失敗

```bash
# 檢查 hugepages
grep HugePages /proc/meminfo

# 檢查 vfio
lsmod | grep vfio

# 檢查設備綁定
dpdk-devbind.py --status
```

### 權限問題

```bash
# DPDK 需要 root 或 capabilities
sudo setcap cap_ipc_lock,cap_sys_rawio+ep ./your-dpdk-app
```

### SSH 中斷

如果不小心綁定了管理網卡：
1. 通過 AWS 控制台連接（EC2 Serial Console 或 Session Manager）
2. 執行 `dpdk-devbind.py --bind=ena <pci_address>`
