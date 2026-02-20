#!/bin/bash
#
# Ubuntu/Debian platform support for DPDK setup
#

os_install_deps() {
    apt-get update -qq
    local deps="build-essential meson ninja-build python3-pyelftools libnuma-dev pkg-config python3-pip wget curl"
    echo "  Packages: $deps"
    apt-get install -y -qq $deps >/dev/null 2>&1
}

os_persist_boot_params() {
    local new_params="$1"

    # EC2 Ubuntu AMI ships 50-cloudimg-settings.cfg which overrides /etc/default/grub
    local grub_conf="/etc/default/grub"
    local cloudimg_conf="/etc/default/grub.d/50-cloudimg-settings.cfg"

    # Determine which file actually controls GRUB_CMDLINE_LINUX_DEFAULT
    local target_file="$grub_conf"
    local target_var="GRUB_CMDLINE_LINUX"

    if [[ -f "$cloudimg_conf" ]]; then
        # EC2 Ubuntu: cloudimg overrides GRUB_CMDLINE_LINUX_DEFAULT
        target_file="$cloudimg_conf"
        target_var="GRUB_CMDLINE_LINUX_DEFAULT"
        log_info "Detected EC2 Ubuntu AMI ($cloudimg_conf)"
    fi

    if [[ ! -f "$target_file" ]]; then
        log_warning "GRUB config not found: $target_file"
        return 1
    fi

    # Read current value
    local current=$(grep "^${target_var}=" "$target_file" | cut -d'"' -f2)

    # Remove old low-latency params
    current=$(echo "$current" | sed -E 's/isolcpus=[^ ]*//g; s/nohz_full=[^ ]*//g; s/rcu_nocbs=[^ ]*//g; s/rcu_nocb_poll//g; s/irqaffinity=[^ ]*//g')
    current=$(echo "$current" | sed -E 's/default_hugepagesz=[^ ]*//g; s/hugepagesz=[^ ]*//g; s/hugepages=[^ ]*//g')
    current=$(echo "$current" | sed -E 's/intel_idle\.max_cstate=[^ ]*//g; s/processor\.max_cstate=[^ ]*//g; s/mitigations=[^ ]*//g; s/idle=poll//g')
    current=$(echo "$current" | tr -s ' ')

    # Combine
    local final="$current $new_params"
    final=$(echo "$final" | sed 's/^ *//; s/ *$//')

    # Update target file
    sed -i "s|^${target_var}=.*|${target_var}=\"$final\"|" "$target_file"

    # Regenerate GRUB
    if command -v update-grub &>/dev/null; then
        update-grub 2>/dev/null
    elif command -v grub2-mkconfig &>/dev/null; then
        grub2-mkconfig -o /boot/grub2/grub.cfg 2>/dev/null
    fi

    log_success "GRUB updated ($target_file)"
}

os_meson_extra_args() {
    # Ubuntu builds all drivers fine on current kernels
    echo ""
}

os_setup_paths() {
    # Standard paths work on Ubuntu
    export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:${PKG_CONFIG_PATH:-}
}
