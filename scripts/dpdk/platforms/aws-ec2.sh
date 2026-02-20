#!/bin/bash
#
# AWS EC2 Platform Script for DPDK
#
# This script generates DPDK environment configuration by querying
# the EC2 Instance Metadata Service (IMDS).
#
# It reads network interface information from:
#   http://169.254.169.254/latest/meta-data/network/interfaces/macs/
#

METADATA_URL="http://169.254.169.254/latest/meta-data"
METADATA_TOKEN_URL="http://169.254.169.254/latest/api/token"

# Get IMDSv2 token
get_token() {
    curl -sf -X PUT "$METADATA_TOKEN_URL" \
        -H "X-aws-ec2-metadata-token-ttl-seconds: 300" 2>/dev/null
}

# Fetch metadata with token (-f: fail silently on HTTP errors like 404)
fetch_metadata() {
    local path="$1"
    local token=$(get_token)

    if [[ -n "$token" ]]; then
        curl -sf -H "X-aws-ec2-metadata-token: $token" \
            "${METADATA_URL}${path}" 2>/dev/null
    else
        curl -sf "${METADATA_URL}${path}" 2>/dev/null
    fi
}

# Get PCI address for a kernel-bound interface
get_pci_address() {
    local ifname="$1"
    local device_path="/sys/class/net/$ifname/device"

    if [[ -L "$device_path" ]]; then
        basename "$(readlink -f "$device_path")"
    fi
}

# Build a map of MAC → PCI for DPDK-bound devices using elimination:
#   1. Collect all kernel-bound MACs from sysfs
#   2. All IMDS MACs not in sysfs must be DPDK-bound
#   3. Sort unmatched MACs by device-number, DPDK PCI addresses ascending → pair 1:1
build_dpdk_mac_pci_map() {
    # Collect kernel-bound MACs
    local kernel_macs=""
    for iface in $(ls /sys/class/net/); do
        if [[ -f "/sys/class/net/$iface/address" ]]; then
            kernel_macs="$kernel_macs $(cat "/sys/class/net/$iface/address")"
        fi
    done

    # Collect all DPDK-bound PCI addresses (sorted ascending)
    local dpdk_pcis=$(dpdk-devbind.py --status 2>/dev/null \
        | grep "drv=vfio-pci" | awk '{print $1}' | sort)

    # Collect IMDS MACs not found in kernel (= DPDK-bound), sorted by device-number
    local unmatched=""  # "device_number:mac" entries
    local all_macs=$(fetch_metadata "/network/interfaces/macs/")
    for mac_slash in $all_macs; do
        local mac="${mac_slash%/}"
        local found=false
        for km in $kernel_macs; do
            if [[ "$km" == "$mac" ]]; then
                found=true
                break
            fi
        done
        if [[ "$found" == "false" ]]; then
            local devnum=$(fetch_metadata "/network/interfaces/macs/$mac/device-number")
            unmatched="$unmatched ${devnum}:${mac}"
        fi
    done
    # Sort by device-number
    unmatched=$(echo "$unmatched" | tr ' ' '\n' | sort -t: -k1 -n | tr '\n' ' ')

    # Pair: sorted unmatched MACs ↔ sorted DPDK PCI addresses
    # Output: "mac=pci" lines (consumed by generate_config)
    local i=0
    local pci_arr=($dpdk_pcis)
    for entry in $unmatched; do
        local mac="${entry#*:}"
        if [[ $i -lt ${#pci_arr[@]} ]]; then
            echo "$mac=${pci_arr[$i]}"
        fi
        ((i++))
    done
}

# Generate configuration JSON
generate_config() {
    local timestamp=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    local instance_type=$(fetch_metadata "/instance-type")

    # Pre-build DPDK MAC→PCI map
    local dpdk_map=$(build_dpdk_mac_pci_map)

    # Load NIC names once for DPDK device name reconstruction
    local -a dpdk_nic_names=()
    local ll_config="/etc/dpdk/low-latency.conf"
    if [[ -f "$ll_config" ]]; then
        local _DPDK_NICS=""
        _DPDK_NICS=$(source "$ll_config" && echo "$DPDK_NICS")
        dpdk_nic_names=($_DPDK_NICS)
    fi

    echo "{"
    echo "  \"version\": 2,"
    echo "  \"generated_at\": \"$timestamp\","
    echo "  \"platform\": \"aws-ec2\","
    echo "  \"metadata\": {"
    echo "    \"instance_type\": \"$instance_type\""
    echo "  },"

    # Generate dpdk_cores array from low-latency.conf if available
    # Otherwise fallback to all cores except 0
    local dpdk_cpus=""

    if [[ -f "$ll_config" ]]; then
        dpdk_cpus=$(source "$ll_config" && echo "$DPDK_CPUS")
    fi

    echo "  \"dpdk_cores\": ["
    local cores_first=true

    if [[ -n "$dpdk_cpus" ]]; then
        # Use configured DPDK CPUs from low-latency.conf
        for cpu in $dpdk_cpus; do
            if [[ "$cores_first" != "true" ]]; then
                echo ","
            fi
            cores_first=false
            echo -n "    $cpu"
        done
    else
        # Fallback: use all cores except 0
        local num_cpus=$(nproc --all)
        for ((i=1; i<num_cpus; i++)); do
            if [[ "$cores_first" != "true" ]]; then
                echo ","
            fi
            cores_first=false
            echo -n "    $i"
        done
    fi
    echo ""
    echo "  ],"

    # Get all MACs from metadata
    local macs=$(fetch_metadata "/network/interfaces/macs/")

    echo "  \"devices\": ["

    local first=true

    for mac_with_slash in $macs; do
        local mac="${mac_with_slash%/}"

        # Get device info from metadata
        local device_number=$(fetch_metadata "/network/interfaces/macs/$mac/device-number")
        local local_ipv4s=$(fetch_metadata "/network/interfaces/macs/$mac/local-ipv4s")
        local local_ipv6s=$(fetch_metadata "/network/interfaces/macs/$mac/ipv6s" 2>/dev/null || echo "")
        local subnet_cidr=$(fetch_metadata "/network/interfaces/macs/$mac/subnet-ipv4-cidr-block")
        local subnet_cidr_v6=$(fetch_metadata "/network/interfaces/macs/$mac/subnet-ipv6-cidr-blocks" 2>/dev/null || echo "")
        local vpc_cidr=$(fetch_metadata "/network/interfaces/macs/$mac/vpc-ipv4-cidr-blocks")

        # Extract prefix length from subnet CIDR
        local prefix="${subnet_cidr#*/}"
        [[ -z "$prefix" ]] && prefix="20"

        # Find PCI address and interface name
        local ifname=""
        local pci=""

        # Try sysfs first (kernel-bound devices)
        for iface in $(ls /sys/class/net/); do
            if [[ -f "/sys/class/net/$iface/address" ]]; then
                local iface_mac=$(cat "/sys/class/net/$iface/address")
                if [[ "$iface_mac" == "$mac" ]]; then
                    ifname="$iface"
                    pci=$(get_pci_address "$iface")
                    break
                fi
            fi
        done

        # If not in sysfs, look up from DPDK elimination map
        if [[ -z "$pci" ]]; then
            local map_entry=$(echo "$dpdk_map" | grep "^$mac=")
            if [[ -n "$map_entry" ]]; then
                pci="${map_entry#*=}"
                # Reconstruct original interface name from pre-loaded NIC names
                local dpdk_idx=0
                for entry in $dpdk_map; do
                    local entry_mac="${entry%=*}"
                    if [[ "$entry_mac" == "$mac" ]]; then
                        break
                    fi
                    ((dpdk_idx++))
                done
                if [[ $dpdk_idx -lt ${#dpdk_nic_names[@]} ]]; then
                    ifname="${dpdk_nic_names[$dpdk_idx]}"
                fi
                [[ -z "$ifname" ]] && ifname="dpdk${device_number}"
            fi
        fi

        # Skip if no PCI address found
        [[ -z "$pci" ]] && continue

        # Determine role from current binding status
        local role="kernel"
        if dpdk-devbind.py --status 2>/dev/null | grep "$pci" | grep -q "drv=vfio-pci\|drv=igb_uio"; then
            role="dpdk"
        fi

        # Calculate gateway (usually .1 of the subnet)
        local gateway_v4=""
        if [[ -n "$subnet_cidr" ]]; then
            local subnet_base="${subnet_cidr%/*}"
            local octets=(${subnet_base//./ })
            gateway_v4="${octets[0]}.${octets[1]}.${octets[2]}.1"
        fi

        # Get IPv6 gateway and prefix from kernel routing table
        local gateway_v6=""
        local prefix_v6="128"
        if [[ -n "$subnet_cidr_v6" ]]; then
            prefix_v6="${subnet_cidr_v6#*/}"
            [[ -z "$prefix_v6" ]] && prefix_v6="64"
            # AWS VPC uses a link-local address as the IPv6 gateway.
            # Read it from the kernel routing table for this interface.
            if [[ -n "$ifname" ]]; then
                gateway_v6=$(ip -6 route show default dev "$ifname" 2>/dev/null | awk '/via/ {print $3; exit}')
            fi
            # Fallback: get from any interface's IPv6 default route
            if [[ -z "$gateway_v6" ]]; then
                gateway_v6=$(ip -6 route show default 2>/dev/null | awk '/via/ {print $3; exit}')
            fi
        fi

        # Output JSON
        if [[ "$first" != "true" ]]; then
            echo ","
        fi
        first=false

        echo "    {"
        echo "      \"pci_address\": \"$pci\","
        echo "      \"mac\": \"$mac\","

        # Build addresses array (IPv4 + IPv6)
        # Only include IPv4 addresses with EIP associations (external connectivity)
        echo "      \"addresses\": ["
        local addr_first=true

        # Get EIP associations for this interface
        local eip_associations=$(fetch_metadata "/network/interfaces/macs/$mac/ipv4-associations/" 2>/dev/null || echo "")
        local ips_with_eip=""

        # Find IPs with EIP association
        for ip in $local_ipv4s; do
            local has_eip=false
            for eip_with_slash in $eip_associations; do
                local eip="${eip_with_slash%/}"
                local associated_private=$(fetch_metadata "/network/interfaces/macs/$mac/ipv4-associations/$eip" 2>/dev/null || echo "")
                if [[ "$associated_private" == "$ip" ]]; then
                    has_eip=true
                    break
                fi
            done

            if [[ "$has_eip" == "true" ]]; then
                ips_with_eip="$ips_with_eip $ip"
            fi
        done

        # Output only IPs with public IP associations
        for ip in $ips_with_eip; do
            if [[ -n "$ip" ]]; then
                if [[ "$addr_first" != "true" ]]; then
                    echo ","
                fi
                addr_first=false
                echo -n "        \"$ip/$prefix\""
            fi
        done
        # Add IPv6 addresses
        for ip in $local_ipv6s; do
            if [[ -n "$ip" ]]; then
                if [[ "$addr_first" != "true" ]]; then
                    echo ","
                fi
                addr_first=false
                echo -n "        \"$ip/$prefix_v6\""
            fi
        done
        echo ""
        echo "      ],"

        [[ -n "$gateway_v4" ]] && echo "      \"gateway_v4\": \"$gateway_v4\","
        [[ -n "$gateway_v6" ]] && echo "      \"gateway_v6\": \"$gateway_v6\","
        echo "      \"mtu\": 9001,"
        echo "      \"role\": \"$role\","
        echo "      \"original_name\": \"$ifname\""
        echo -n "    }"
    done

    echo ""
    echo "  ],"

    # Hugepages config
    local hugepages_total=$(cat /proc/meminfo | grep HugePages_Total | awk '{print $2}')
    [[ -z "$hugepages_total" || "$hugepages_total" -eq 0 ]] && hugepages_total="${DPDK_HUGEPAGES:-512}"

    echo "  \"hugepages\": {"
    echo "    \"size_kb\": 2048,"
    echo "    \"count\": $hugepages_total,"
    echo "    \"mount\": \"/mnt/huge\""
    echo "  },"

    # EAL args for AWS
    echo "  \"eal_args\": ["
    echo "    \"--iova-mode=pa\""
    echo "  ]"

    echo "}"
}

# If sourced, functions are available
# If executed directly, run generate_config
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    generate_config
fi
