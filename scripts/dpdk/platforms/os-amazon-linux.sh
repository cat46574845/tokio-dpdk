#!/bin/bash
#
# Amazon Linux 2023 / RHEL / Fedora platform support for DPDK setup
#

os_install_deps() {
    # curl-minimal is pre-installed on AL2023 and conflicts with full curl
    local deps="gcc gcc-c++ make numactl-devel pkgconf-pkg-config python3-pip wget libpcap-devel"
    echo "  Packages: $deps"
    dnf install -y $deps >/dev/null 2>&1
    # meson/ninja/pyelftools not in default repos, install via pip
    pip3 install meson pyelftools ninja >/dev/null 2>&1
    export PATH=/usr/local/bin:/root/.local/bin:$PATH
}

os_persist_boot_params() {
    local new_params="$1"

    # BLS-based boot: grubby manages per-kernel boot entries directly
    # /etc/default/grub + grub2-mkconfig does NOT work with GRUB_ENABLE_BLSCFG=true
    local old_params="isolcpus nohz_full rcu_nocbs rcu_nocb_poll irqaffinity default_hugepagesz hugepagesz hugepages intel_idle.max_cstate processor.max_cstate mitigations idle"
    local remove_args=""
    for p in $old_params; do
        local current_val=$(grubby --info=DEFAULT 2>/dev/null | grep "^args=" | grep -oP "$p=[^ \"]*" || true)
        [[ -n "$current_val" ]] && remove_args="$remove_args --remove-args=$current_val"
        if grubby --info=DEFAULT 2>/dev/null | grep "^args=" | grep -qw "$p" 2>/dev/null; then
            remove_args="$remove_args --remove-args=$p"
        fi
    done
    [[ -n "$remove_args" ]] && grubby --update-kernel=ALL $remove_args 2>/dev/null || true

    # Only add --args if new_params is non-empty (supports uninstall with empty string)
    if [[ -n "$new_params" ]]; then
        grubby --update-kernel=ALL --args="$new_params"
    fi

    log_success "Boot parameters updated via grubby"
}

os_meson_extra_args() {
    # AL2023 kernel headers redefine __le64/__be64 in linux/types.h
    # which conflicts with DPDK's gve/fm10k/other NIC driver typedefs.
    # On EC2 we only need ENA anyway.
    echo "-Denable_drivers=net/ena"
}

os_setup_paths() {
    # pip3-installed tools go to /usr/local/bin or ~/.local/bin
    export PATH=/usr/local/bin:/root/.local/bin:$PATH
    # DPDK installs libs to lib64 on 64-bit RHEL-family
    export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig:${PKG_CONFIG_PATH:-}
}
