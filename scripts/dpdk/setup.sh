#!/bin/bash
#
# Unified DPDK + Low Latency Environment Setup
#
# This script provides a one-stop solution for configuring:
# 1. CPU core isolation for DPDK
# 2. DPDK installation and device binding
# 3. System-wide low latency tuning (for non-DPDK devices)
#
# The goal is to ensure DPDK cores are NEVER disturbed by:
# - Scheduler (via isolcpus)
# - Timer ticks (via nohz_full)
# - RCU callbacks (via rcu_nocbs)
# - IRQs from ANY device (via irqaffinity + manual binding)
# - Other processes
#
# Usage:
#   ./setup.sh wizard       - Interactive one-stop setup for new machines
#   ./setup.sh detect       - Detect hardware and show recommendations
#   ./setup.sh configure    - Generate configuration files
#   ./setup.sh apply        - Apply runtime configuration
#   ./setup.sh persist      - Make configuration permanent (GRUB, systemd)
#   ./setup.sh dpdk-install - Install DPDK from source
#   ./setup.sh dpdk-bind    - Bind NICs to DPDK
#   ./setup.sh verify       - Verify setup

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Configuration paths
CONFIG_DIR="/etc/dpdk"
CONFIG_FILE="$CONFIG_DIR/env.json"
LL_CONFIG_FILE="$CONFIG_DIR/low-latency.conf"
GRUB_CONF="/etc/default/grub"
SYSCTL_CONF="/etc/sysctl.d/99-dpdk-low-latency.conf"
SERVICE_FILE="/etc/systemd/system/dpdk-low-latency.service"

# Defaults
DPDK_VERSION="${DPDK_VERSION:-23.11}"
DPDK_PREFIX="${DPDK_PREFIX:-/usr/local}"
HUGEPAGES="${DPDK_HUGEPAGES:-512}"

# Colors and styles
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
MAGENTA='\033[0;35m'
WHITE='\033[1;37m'
GRAY='\033[0;90m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

# Icons (unicode symbols, no emoji)
ICON_OK="${GREEN}✓${NC}"
ICON_FAIL="${RED}✗${NC}"
ICON_WARN="${YELLOW}!${NC}"
ICON_INFO="${BLUE}*${NC}"
ICON_ARROW="${CYAN}→${NC}"
ICON_GEAR="${MAGENTA}⚙${NC}"
ICON_ROCKET="${CYAN}»${NC}"
ICON_PACKAGE="${GREEN}■${NC}"
ICON_NETWORK="${BLUE}◆${NC}"
ICON_CPU="${MAGENTA}●${NC}"
ICON_MEMORY="${YELLOW}▣${NC}"
ICON_STATUS="${CYAN}◈${NC}"

log_info()    { echo -e "  ${ICON_INFO}  $1"; }
log_success() { echo -e "  ${ICON_OK}  $1"; }
log_warning() { echo -e "  ${ICON_WARN}  ${YELLOW}$1${NC}"; }
log_error()   { echo -e "  ${ICON_FAIL}  ${RED}$1${NC}"; }
log_section() { 
    echo ""
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BOLD}${WHITE}  $1${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""
}
log_step() { echo -e "${GRAY}────────────────────────────────────────${NC}"; }

check_root() {
    if [[ $EUID -ne 0 ]]; then
        log_error "This command requires root privileges. Use sudo."
        exit 1
    fi
}

# =============================================================================
# Hardware Detection
# =============================================================================

detect_platform() {
    if curl -s --max-time 1 http://169.254.169.254/latest/meta-data/ &>/dev/null; then
        echo "aws-ec2"
    else
        echo "bare-metal"
    fi
}

detect_cpus() {
    declare -ga ALL_CPUS
    local num_cpus=$(nproc --all)
    for i in $(seq 0 $((num_cpus - 1))); do
        ALL_CPUS+=($i)
    done
    echo "Detected CPUs: ${ALL_CPUS[*]}"
}

detect_nics() {
    declare -ga ALL_NICS
    declare -g PRIMARY_NIC
    
    # Find primary NIC (used for SSH/default route)
    PRIMARY_NIC=$(ip route show default 2>/dev/null | head -1 | awk '{print $5}')
    
    # Find all physical NICs
    for iface in $(ls /sys/class/net/ | grep -v lo); do
        # Skip virtual interfaces
        if [[ -d "/sys/devices/virtual/net/$iface" ]]; then
            continue
        fi
        ALL_NICS+=("$iface")
    done
    
    echo "Detected NICs: ${ALL_NICS[*]}"
    echo "Primary NIC (SSH): $PRIMARY_NIC"
}

cmd_detect() {
    log_section "Hardware Detection"
    
    local platform=$(detect_platform)
    echo "Platform: $platform"
    echo ""
    
    detect_cpus
    echo ""
    detect_nics
    echo ""
    
    # Recommendations
    log_section "Recommendations"
    
    local num_cpus=${#ALL_CPUS[@]}
    local num_nics=${#ALL_NICS[@]}
    
    # Recommend leaving CPU 0 for system
    echo "CPU Allocation:"
    echo "  - CPU 0: System/Kernel (recommended to avoid)"
    if [[ $num_cpus -ge 4 ]]; then
        echo "  - CPU 1-$((num_cpus-1)): Available for DPDK"
    fi
    echo ""
    
    echo "NIC Allocation:"
    echo "  - $PRIMARY_NIC: Keep for SSH/management (DO NOT bind to DPDK)"
    for nic in "${ALL_NICS[@]}"; do
        if [[ "$nic" != "$PRIMARY_NIC" ]]; then
            local pci=$(readlink -f "/sys/class/net/$nic/device" | xargs basename)
            echo "  - $nic ($pci): Available for DPDK"
        fi
    done
}

# =============================================================================
# Interactive Wizard
# =============================================================================

read_input() {
    local prompt="$1"
    local default="$2"
    local result
    
    if [[ -n "$default" ]]; then
        read -p "$prompt [$default]: " result
        echo "${result:-$default}"
    else
        read -p "$prompt: " result
        echo "$result"
    fi
}

read_yn() {
    local prompt="$1"
    local default="$2"
    local result
    
    if [[ "$default" == "Y" ]]; then
        read -p "$prompt [Y/n]: " result
        result="${result:-Y}"
    else
        read -p "$prompt [y/N]: " result
        result="${result:-N}"
    fi
    
    [[ "${result^^}" == "Y" ]] && echo "yes" || echo "no"
}

cmd_wizard() {
    check_root
    
    log_section "DPDK + Low Latency Setup Wizard"
    echo "This wizard will configure your system for optimal DPDK performance."
    echo ""
    
    # Step 1: Detect hardware
    log_section "Step 1: Hardware Detection"
    detect_cpus
    detect_nics
    echo ""
    
    # Step 2: CPU allocation
    log_section "Step 2: CPU Core Allocation"
    echo "Available CPUs: ${ALL_CPUS[*]}"
    echo ""
    echo "Note: CPU 0 should be avoided for DPDK (used by kernel)."
    echo ""
    
    local dpdk_cpus=$(read_input "Enter CPUs for DPDK (comma-separated, e.g., 1,2,3)" "1")
    IFS=',' read -ra DPDK_CPUS <<< "$dpdk_cpus"
    
    # Calculate non-DPDK CPUs (for IRQs and system)
    declare -a SYSTEM_CPUS
    for cpu in "${ALL_CPUS[@]}"; do
        local is_dpdk=false
        for dc in "${DPDK_CPUS[@]}"; do
            [[ "$cpu" == "$dc" ]] && is_dpdk=true
        done
        [[ "$is_dpdk" == "false" ]] && SYSTEM_CPUS+=("$cpu")
    done
    
    echo ""
    echo -e "DPDK cores: ${GREEN}${DPDK_CPUS[*]}${NC}"
    echo -e "System cores (IRQs, kernel): ${YELLOW}${SYSTEM_CPUS[*]}${NC}"
    echo ""
    
    # Step 3: NIC allocation
    log_section "Step 3: NIC Allocation"
    echo "Available NICs:"
    for nic in "${ALL_NICS[@]}"; do
        local pci=$(readlink -f "/sys/class/net/$nic/device" 2>/dev/null | xargs basename)
        local ip=$(ip -4 addr show "$nic" 2>/dev/null | grep inet | head -1 | awk '{print $2}')
        local primary_tag=""
        [[ "$nic" == "$PRIMARY_NIC" ]] && primary_tag="${YELLOW}[PRIMARY - SSH]${NC} "
        echo -e "  ${primary_tag}$nic ($pci) - $ip"
    done
    echo ""
    
    log_warning "Do NOT bind the primary NIC ($PRIMARY_NIC) to DPDK or you will lose SSH!"
    echo ""
    
    # Auto-suggest: all NICs except primary
    local suggested_nics=""
    for nic in "${ALL_NICS[@]}"; do
        [[ "$nic" != "$PRIMARY_NIC" ]] && suggested_nics="$suggested_nics$nic,"
    done
    suggested_nics="${suggested_nics%,}"  # Remove trailing comma
    
    local dpdk_nics=$(read_input "Enter NICs for DPDK (comma-separated)" "$suggested_nics")
    IFS=',' read -ra DPDK_NICS <<< "$dpdk_nics"
    
    # Kernel NICs = all NICs - DPDK NICs
    declare -a KERNEL_NICS
    for nic in "${ALL_NICS[@]}"; do
        local is_dpdk=false
        for dn in "${DPDK_NICS[@]}"; do
            [[ "$nic" == "$dn" ]] && is_dpdk=true
        done
        [[ "$is_dpdk" == "false" ]] && KERNEL_NICS+=("$nic")
    done
    
    echo ""
    echo -e "DPDK NICs: ${GREEN}${DPDK_NICS[*]}${NC}"
    echo -e "Kernel NICs: ${YELLOW}${KERNEL_NICS[*]}${NC}"
    echo ""
    
    # Step 4: Hugepages
    log_section "Step 4: Hugepages Configuration"
    local hugepages=$(read_input "Number of 2MB hugepages" "$HUGEPAGES")
    echo ""
    
    # Step 5: Confirm
    log_section "Configuration Summary"
    echo "Platform: $(detect_platform)"
    echo ""
    echo "CPU Allocation:"
    echo "  DPDK cores: ${DPDK_CPUS[*]}"
    echo "  System cores: ${SYSTEM_CPUS[*]}"
    echo ""
    echo "NIC Allocation:"
    echo "  DPDK NICs: ${DPDK_NICS[*]}"
    echo "  Kernel NICs: ${KERNEL_NICS[*]}"
    echo ""
    echo "Memory:"
    echo "  Hugepages: $hugepages x 2MB"
    echo ""
    echo "GRUB Parameters (will be added):"
    local isolcpus=$(IFS=,; echo "${DPDK_CPUS[*]}")
    local irqaffinity=$(IFS=,; echo "${SYSTEM_CPUS[*]}")
    echo "  isolcpus=$isolcpus nohz_full=$isolcpus rcu_nocbs=$isolcpus irqaffinity=$irqaffinity"
    echo ""
    
    local confirm=$(read_yn "Proceed with this configuration?" "Y")
    if [[ "$confirm" != "yes" ]]; then
        echo "Aborted."
        exit 0
    fi
    
    # Save configuration
    mkdir -p "$CONFIG_DIR"
    cat > "$LL_CONFIG_FILE" << EOF
# DPDK + Low Latency Configuration
# Generated by setup.sh wizard

DPDK_CPUS="${DPDK_CPUS[*]}"
SYSTEM_CPUS="${SYSTEM_CPUS[*]}"
DPDK_NICS="${DPDK_NICS[*]}"
KERNEL_NICS="${KERNEL_NICS[*]}"
HUGEPAGES="$hugepages"
ISOLCPUS="$isolcpus"
IRQAFFINITY="$irqaffinity"
EOF
    
    log_success "Configuration saved to $LL_CONFIG_FILE"
    
    # Execute steps
    echo ""
    local install_dpdk=$(read_yn "Install DPDK $DPDK_VERSION now?" "Y")
    if [[ "$install_dpdk" == "yes" ]]; then
        cmd_dpdk_install
    fi
    
    echo ""
    local apply_now=$(read_yn "Apply runtime configuration now?" "Y")
    if [[ "$apply_now" == "yes" ]]; then
        cmd_apply
    fi
    
    echo ""
    local persist=$(read_yn "Make configuration persistent (GRUB + systemd)?" "Y")
    if [[ "$persist" == "yes" ]]; then
        cmd_persist
    fi
    
    echo ""
    local bind_dpdk=$(read_yn "Bind NICs to DPDK now?" "Y")
    if [[ "$bind_dpdk" == "yes" ]]; then
        cmd_dpdk_bind
    fi
    
    log_section "Setup Complete"
    echo "DPDK environment is configured!"
    echo ""
    if [[ "$persist" == "yes" ]]; then
        echo -e "${YELLOW}IMPORTANT: Reboot required for GRUB parameters to take effect.${NC}"
        local reboot_now=$(read_yn "Reboot now?" "N")
        if [[ "$reboot_now" == "yes" ]]; then
            echo "Rebooting in 5 seconds..."
            sleep 5
            reboot
        fi
    fi
}

# =============================================================================
# Apply Runtime Configuration
# =============================================================================

cmd_apply() {
    check_root
    log_section "Applying Runtime Configuration"
    
    # Load configuration
    if [[ ! -f "$LL_CONFIG_FILE" ]]; then
        log_error "Configuration not found: $LL_CONFIG_FILE"
        log_error "Run '$0 wizard' or '$0 configure' first."
        exit 1
    fi
    source "$LL_CONFIG_FILE"
    
    # 1. Stop irqbalance
    log_info "Stopping irqbalance..."
    systemctl stop irqbalance 2>/dev/null || true
    systemctl disable irqbalance 2>/dev/null || true
    log_success "irqbalance stopped"
    
    # 2. Disable Transparent Huge Pages
    log_info "Disabling Transparent Huge Pages..."
    echo never > /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null || true
    echo never > /sys/kernel/mm/transparent_hugepage/defrag 2>/dev/null || true
    log_success "THP disabled"
    
    # 3. Configure hugepages
    log_info "Configuring hugepages ($HUGEPAGES x 2MB)..."
    echo "$HUGEPAGES" > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
    mkdir -p /mnt/huge
    mount -t hugetlbfs nodev /mnt/huge 2>/dev/null || true
    log_success "Hugepages configured"
    
    # 4. Apply sysctl
    log_info "Applying sysctl parameters..."
    sysctl -w vm.swappiness=0 >/dev/null
    sysctl -w kernel.numa_balancing=0 >/dev/null
    sysctl -w kernel.sched_rt_runtime_us=-1 >/dev/null 2>&1 || true
    log_success "sysctl applied"
    
    # 5. CPU frequency (performance mode)
    log_info "Setting CPU governor to performance..."
    if command -v cpupower &>/dev/null; then
        cpupower frequency-set -g performance >/dev/null 2>&1 || true
        log_success "CPU governor set to performance"
    else
        log_warning "cpupower not available, skipping"
    fi
    
    # 6. Bind IRQs for KERNEL NICs to system cores
    log_info "Binding kernel NIC IRQs to system cores..."
    local system_cpus_list="${SYSTEM_CPUS// /,}"
    for nic in $KERNEL_NICS; do
        if [[ -d "/sys/class/net/$nic" ]]; then
            local irqs=$(grep "$nic" /proc/interrupts 2>/dev/null | awk -F: '{print $1}' | tr -d ' ')
            for irq in $irqs; do
                echo "$system_cpus_list" > /proc/irq/$irq/smp_affinity_list 2>/dev/null || true
            done
            log_success "  $nic IRQs -> CPU $system_cpus_list"
        fi
    done
    
    # 7. Bind ALL other device IRQs to system cores
    log_info "Binding other device IRQs to system cores..."
    for irq_dir in /proc/irq/*/; do
        irq=$(basename "$irq_dir")
        [[ "$irq" =~ ^[0-9]+$ ]] || continue
        [[ "$irq" == "0" ]] && continue  # Skip timer
        echo "$system_cpus_list" > /proc/irq/$irq/smp_affinity_list 2>/dev/null || true
    done
    log_success "All device IRQs bound to system cores"
    
    log_section "Runtime Configuration Applied"
}

# =============================================================================
# Persist Configuration
# =============================================================================

cmd_persist() {
    check_root
    log_section "Persisting Configuration"
    
    # Load configuration
    if [[ ! -f "$LL_CONFIG_FILE" ]]; then
        log_error "Configuration not found: $LL_CONFIG_FILE"
        exit 1
    fi
    source "$LL_CONFIG_FILE"
    
    # 1. Update GRUB
    log_info "Updating GRUB configuration..."
    if [[ -f "$GRUB_CONF" ]]; then
        # Build new parameters
        local new_params="isolcpus=$ISOLCPUS nohz_full=$ISOLCPUS rcu_nocbs=$ISOLCPUS rcu_nocb_poll irqaffinity=$IRQAFFINITY"
        new_params="$new_params default_hugepagesz=2M hugepagesz=2M hugepages=$HUGEPAGES"
        
        # Read current GRUB_CMDLINE_LINUX
        local current=$(grep "^GRUB_CMDLINE_LINUX=" "$GRUB_CONF" | cut -d'"' -f2)
        
        # Remove old low-latency params
        current=$(echo "$current" | sed -E 's/isolcpus=[^ ]*//g; s/nohz_full=[^ ]*//g; s/rcu_nocbs=[^ ]*//g; s/rcu_nocb_poll//g; s/irqaffinity=[^ ]*//g')
        current=$(echo "$current" | sed -E 's/default_hugepagesz=[^ ]*//g; s/hugepagesz=[^ ]*//g; s/hugepages=[^ ]*//g')
        current=$(echo "$current" | tr -s ' ')
        
        # Combine
        local final="$current $new_params"
        final=$(echo "$final" | sed 's/^ *//; s/ *$//')
        
        # Update GRUB
        sed -i "s|^GRUB_CMDLINE_LINUX=.*|GRUB_CMDLINE_LINUX=\"$final\"|" "$GRUB_CONF"
        
        # Regenerate GRUB
        if command -v update-grub &>/dev/null; then
            update-grub 2>/dev/null
        elif command -v grub2-mkconfig &>/dev/null; then
            grub2-mkconfig -o /boot/grub2/grub.cfg 2>/dev/null
        fi
        
        log_success "GRUB updated"
    else
        log_warning "GRUB config not found: $GRUB_CONF"
    fi
    
    # 2. Create sysctl config
    log_info "Creating sysctl configuration..."
    cat > "$SYSCTL_CONF" << EOF
# DPDK + Low Latency sysctl configuration
# Generated by setup.sh

vm.swappiness = 0
kernel.numa_balancing = 0
EOF
    log_success "Created $SYSCTL_CONF"
    
    # 3. Create systemd service
    log_info "Creating systemd service..."
    cat > "$SERVICE_FILE" << EOF
[Unit]
Description=DPDK Low Latency Configuration
After=network.target

[Service]
Type=oneshot
ExecStart=$SCRIPT_DIR/setup.sh apply
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF
    
    systemctl daemon-reload
    systemctl enable dpdk-low-latency.service
    log_success "Created and enabled $SERVICE_FILE"
    
    log_section "Configuration Persisted"
    log_warning "Reboot required for GRUB parameters to take effect!"
}

# =============================================================================
# DPDK Installation
# =============================================================================

cmd_dpdk_install() {
    check_root
    log_section "Installing DPDK $DPDK_VERSION"
    
    echo "Installation steps:"
    echo "  1. Install build dependencies"
    echo "  2. Download DPDK source (https://fast.dpdk.org)"
    echo "  3. Configure with meson"
    echo "  4. Build with ninja"
    echo "  5. Install to $DPDK_PREFIX"
    echo ""
    
    # Step 1: Install dependencies
    log_info "[1/5] Installing build dependencies..."
    apt-get update -qq
    local deps="build-essential meson ninja-build python3-pyelftools libnuma-dev pkg-config python3-pip wget curl"
    echo "  Packages: $deps"
    apt-get install -y -qq $deps >/dev/null 2>&1
    log_success "Dependencies installed"
    
    # Check if already installed
    if pkg-config --exists libdpdk 2>/dev/null; then
        local version=$(pkg-config --modversion libdpdk)
        log_warning "DPDK $version already installed at $(pkg-config --variable=prefix libdpdk)"
        local reinstall=$(read_yn "Reinstall?" "N")
        [[ "$reinstall" != "yes" ]] && return 0
    fi
    
    # Step 2: Download
    local work_dir="/tmp/dpdk-build-$$"
    mkdir -p "$work_dir"
    cd "$work_dir"
    
    log_info "[2/5] Downloading DPDK $DPDK_VERSION..."
    local url="https://fast.dpdk.org/rel/dpdk-${DPDK_VERSION}.tar.xz"
    echo "  URL: $url"
    wget --progress=bar:force "$url" 2>&1 | tail -1
    log_success "Downloaded $(du -h dpdk-${DPDK_VERSION}.tar.xz | cut -f1)"
    
    # Step 3: Extract and configure
    log_info "[3/5] Extracting and configuring..."
    tar -xf "dpdk-${DPDK_VERSION}.tar.xz"
    cd "dpdk-${DPDK_VERSION}"
    echo "  Source directory: $(pwd)"
    echo "  Install prefix: $DPDK_PREFIX"
    meson setup build --prefix="$DPDK_PREFIX" -Ddefault_library=shared 2>&1 | tail -5
    log_success "Configuration complete"
    
    # Step 4: Build
    log_info "[4/5] Building (using $(nproc) CPU cores, this may take 5-10 minutes)..."
    local start_time=$(date +%s)
    ninja -C build -j$(nproc) 2>&1 | grep -E "(Compiling|Linking|ninja:)" | tail -10
    local end_time=$(date +%s)
    local duration=$((end_time - start_time))
    log_success "Build complete in ${duration}s"
    
    # Step 5: Install
    log_info "[5/5] Installing to $DPDK_PREFIX..."
    ninja -C build install >/dev/null 2>&1
    ldconfig
    echo "  Libraries: $(pkg-config --libs-only-L libdpdk 2>/dev/null || echo 'N/A')"
    echo "  Headers: $(pkg-config --variable=includedir libdpdk 2>/dev/null || echo 'N/A')"
    log_success "Installation complete"
    
    # Cleanup
    cd /
    rm -rf "$work_dir"
    
    # Verify
    log_section "Installation Summary"
    echo "DPDK Version: $(pkg-config --modversion libdpdk)"
    echo "Install Prefix: $(pkg-config --variable=prefix libdpdk)"
    echo "pkg-config: $(which pkg-config) (libdpdk found: yes)"
    echo ""
    echo "Tools installed:"
    for tool in dpdk-devbind.py dpdk-testpmd dpdk-hugepages.py; do
        if [[ -x "$DPDK_PREFIX/bin/$tool" ]]; then
            echo "  $DPDK_PREFIX/bin/$tool"
        fi
    done
}

# =============================================================================
# DPDK Device Binding
# =============================================================================

cmd_dpdk_bind() {
    check_root
    log_section "Binding NICs to DPDK"
    
    # Load configuration
    if [[ ! -f "$LL_CONFIG_FILE" ]]; then
        log_error "Configuration not found. Run wizard first."
        exit 1
    fi
    source "$LL_CONFIG_FILE"
    
    # Load vfio-pci
    log_info "Loading vfio-pci module..."
    modprobe vfio-pci
    
    # Enable no-IOMMU mode if needed
    if [[ ! -d /sys/class/iommu ]] || [[ -z "$(ls -A /sys/class/iommu 2>/dev/null)" ]]; then
        log_info "Enabling vfio no-IOMMU mode..."
        echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode
    fi
    
    # Bind each DPDK NIC
    for nic in $DPDK_NICS; do
        if [[ ! -d "/sys/class/net/$nic" ]]; then
            log_warning "$nic not found, may already be bound to DPDK"
            continue
        fi
        
        local pci=$(readlink -f "/sys/class/net/$nic/device" | xargs basename)
        
        log_info "Binding $nic ($pci) to vfio-pci..."
        
        # Bring down interface
        ip link set "$nic" down 2>/dev/null || true
        
        # Bind to vfio-pci
        dpdk-devbind.py --bind=vfio-pci "$pci" 2>/dev/null || \
            "$DPDK_PREFIX/bin/dpdk-devbind.py" --bind=vfio-pci "$pci"
        
        log_success "  $nic ($pci) bound to vfio-pci"
    done
    
    # Generate env.json
    log_info "Generating DPDK environment configuration..."
    source "$SCRIPT_DIR/platforms/aws-ec2.sh"
    generate_config > "$CONFIG_FILE"
    log_success "Configuration written to $CONFIG_FILE"
}

# =============================================================================
# Verify Setup
# =============================================================================

cmd_verify() {
    log_section "DPDK + Low Latency Verification"
    
    # DPDK
    echo -e "${BOLD}${ICON_PACKAGE} DPDK Installation${NC}"
    if pkg-config --exists libdpdk 2>/dev/null; then
        local version=$(pkg-config --modversion libdpdk)
        local prefix=$(pkg-config --variable=prefix libdpdk)
        echo -e "  ${ICON_OK}  Version: ${GREEN}${version}${NC}"
        echo -e "      Prefix:  ${DIM}${prefix}${NC}"
    else
        echo -e "  ${ICON_FAIL}  ${RED}Not installed${NC}"
    fi
    echo ""
    
    # Hugepages
    echo -e "${BOLD}${ICON_MEMORY} Hugepages${NC}"
    local total=$(grep HugePages_Total /proc/meminfo | awk '{print $2}')
    local free=$(grep HugePages_Free /proc/meminfo | awk '{print $2}')
    local used=$((total - free))
    if [[ $total -gt 0 ]]; then
        echo -e "  ${ICON_OK}  Total: ${GREEN}${total}${NC} pages (${GREEN}$((total * 2))MB${NC})"
        echo -e "      Used:  ${used}  Free: ${free}"
    else
        echo -e "  ${ICON_FAIL}  ${RED}No hugepages allocated!${NC}"
    fi
    echo ""
    
    # VFIO
    echo -e "${BOLD}${ICON_GEAR} VFIO Module${NC}"
    if lsmod | grep -q vfio_pci; then
        echo -e "  ${ICON_OK}  vfio-pci: ${GREEN}loaded${NC}"
        local noiommu=$(cat /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null || echo "N")
        if [[ "$noiommu" == "Y" ]]; then
            echo -e "      no-IOMMU mode: ${YELLOW}enabled${NC}"
        fi
    else
        echo -e "  ${ICON_FAIL}  vfio-pci: ${RED}not loaded${NC}"
    fi
    echo ""
    
    # Kernel parameters
    echo -e "${BOLD}${ICON_CPU} Kernel Parameters${NC}"
    local cmdline=$(cat /proc/cmdline)
    
    for param in isolcpus nohz_full rcu_nocbs irqaffinity; do
        if echo "$cmdline" | grep -q "$param="; then
            local val=$(echo "$cmdline" | grep -o "$param=[^ ]*" | cut -d= -f2)
            echo -e "  ${ICON_OK}  ${param}: ${GREEN}${val}${NC}"
        else
            echo -e "  ${ICON_WARN}  ${param}: ${YELLOW}not set${NC}"
        fi
    done
    echo ""
    
    # Device bindings
    echo -e "${BOLD}${ICON_NETWORK} Device Bindings${NC}"
    echo ""
    dpdk-devbind.py --status 2>/dev/null | head -20 || \
        "$DPDK_PREFIX/bin/dpdk-devbind.py" --status 2>/dev/null | head -20
}

# =============================================================================
# Refresh Configuration (IP changes)
# =============================================================================

cmd_refresh_config() {
    check_root
    log_section "Refreshing DPDK Environment Configuration"
    
    echo "This command regenerates /etc/dpdk/env.json with current IP addresses"
    echo "from AWS Metadata Service. Use this after:"
    echo "  - Adding/removing Elastic IPs"
    echo "  - Adding/removing secondary private IPs"
    echo "  - Any ENI IP configuration changes"
    echo ""
    
    local platform=$(detect_platform)
    if [[ "$platform" != "aws-ec2" ]]; then
        log_error "This command is only supported on AWS EC2"
        log_info "For other platforms, manually edit $CONFIG_FILE"
        exit 1
    fi
    
    # Backup current config
    if [[ -f "$CONFIG_FILE" ]]; then
        local backup="${CONFIG_FILE}.bak.$(date +%Y%m%d_%H%M%S)"
        cp "$CONFIG_FILE" "$backup"
        log_info "Backed up current config to $backup"
    fi
    
    # Regenerate from AWS metadata
    log_info "Querying AWS Metadata Service..."
    mkdir -p "$CONFIG_DIR"
    source "$SCRIPT_DIR/platforms/aws-ec2.sh"
    generate_config > "$CONFIG_FILE"
    
    # Show what changed
    log_success "Configuration refreshed: $CONFIG_FILE"
    echo ""
    echo "Current IP configuration:"
    grep -A 20 '"devices"' "$CONFIG_FILE" | grep -E '(pci_address|mac|addresses|original_name)' | head -30
    echo ""
    
    log_info "Note: If your DPDK application is running, you may need to restart it"
    log_info "to pick up the new IP configuration."
}

# =============================================================================
# Help
# =============================================================================

show_banner() {
    echo -e "${CYAN}"
    echo '    ____  ____  ____  __ __    ____       __            '
    echo '   / __ \/ __ \/ __ \/ //_/   / __/___   / /_ __  __ ___'
    echo '  / / / / /_/ / / / / ,<     _\ \ / _ \ / __// / / // _ \'
    echo ' / /_/ / ____/ /_/ / /| |   /___//  __// /_ / /_/ // ___/'
    echo '/_____/_/   /_____/_/ |_|        \___/ \__/ \__,_// .__/ '
    echo '                                                 /_/     '
    echo -e "${NC}"
    echo -e "${DIM}Low Latency Environment Configuration Tool${NC}"
    echo ""
}

show_help() {
    show_banner
    
    echo -e "${BOLD}${WHITE}Usage:${NC} $0 ${CYAN}<command>${NC}"
    echo ""
    
    echo -e "${BOLD}${BLUE}${ICON_ROCKET} Initial Setup${NC}"
    echo -e "  ${GREEN}wizard${NC}         ${GRAY}─${NC} Interactive one-stop setup for new machines"
    echo -e "  ${GREEN}detect${NC}         ${GRAY}─${NC} Detect hardware and show recommendations"
    echo ""
    
    echo -e "${BOLD}${BLUE}${ICON_PACKAGE} Installation${NC}"
    echo -e "  ${GREEN}dpdk-install${NC}   ${GRAY}─${NC} Download and install DPDK from source"
    echo -e "  ${GREEN}dpdk-bind${NC}      ${GRAY}─${NC} Bind NICs to DPDK driver (vfio-pci)"
    echo ""
    
    echo -e "${BOLD}${BLUE}${ICON_GEAR} Configuration${NC}"
    echo -e "  ${GREEN}apply${NC}          ${GRAY}─${NC} Apply runtime configuration (no reboot)"
    echo -e "  ${GREEN}persist${NC}        ${GRAY}─${NC} Make configuration permanent (GRUB + systemd)"
    echo -e "  ${GREEN}refresh-config${NC} ${GRAY}─${NC} Refresh IP configuration from AWS metadata"
    echo ""
    
    echo -e "${BOLD}${BLUE}${ICON_STATUS} Status${NC}"
    echo -e "  ${GREEN}verify${NC}         ${GRAY}─${NC} Verify current setup"
    echo ""
    
    echo -e "${BOLD}${WHITE}Environment Variables:${NC}"
    echo -e "  ${YELLOW}DPDK_VERSION${NC}   ${GRAY}─${NC} DPDK version to install ${DIM}(default: ${DPDK_VERSION})${NC}"
    echo -e "  ${YELLOW}DPDK_PREFIX${NC}    ${GRAY}─${NC} Installation prefix ${DIM}(default: ${DPDK_PREFIX})${NC}"
    echo -e "  ${YELLOW}DPDK_HUGEPAGES${NC} ${GRAY}─${NC} Number of 2MB hugepages ${DIM}(default: ${HUGEPAGES})${NC}"
    echo ""
    
    echo -e "${DIM}Example: sudo $0 wizard${NC}"
    echo ""
}

# =============================================================================
# Main
# =============================================================================

case "${1:-}" in
    wizard)         cmd_wizard ;;
    detect)         cmd_detect ;;
    configure)      cmd_wizard ;;  # Same as wizard for now
    apply)          cmd_apply ;;
    persist)        cmd_persist ;;
    dpdk-install)   cmd_dpdk_install ;;
    dpdk-bind)      cmd_dpdk_bind ;;
    refresh-config) cmd_refresh_config ;;
    verify)         cmd_verify ;;
    -h|--help|help|"") show_help ;;
    *)              show_help; exit 1 ;;
esac
