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
    curl -s -X PUT "$METADATA_TOKEN_URL" \
        -H "X-aws-ec2-metadata-token-ttl-seconds: 300" 2>/dev/null
}

# Fetch metadata with token
fetch_metadata() {
    local path="$1"
    local token=$(get_token)
    
    if [[ -n "$token" ]]; then
        # IMDSv2
        curl -s -H "X-aws-ec2-metadata-token: $token" \
            "${METADATA_URL}${path}" 2>/dev/null
    else
        # Fallback to IMDSv1
        curl -s "${METADATA_URL}${path}" 2>/dev/null
    fi
}

# Get PCI address for an interface
get_pci_address() {
    local ifname="$1"
    local device_path="/sys/class/net/$ifname/device"
    
    if [[ -L "$device_path" ]]; then
        basename "$(readlink -f "$device_path")"
    fi
}

# Get MAC address from interface or from dpdk-devbind
get_mac_for_pci() {
    local pci="$1"
    
    # First try to get from sysfs (if still bound to kernel)
    local ifname_path="/sys/bus/pci/devices/$pci/net"
    if [[ -d "$ifname_path" ]]; then
        local ifname=$(ls "$ifname_path" | head -1)
        if [[ -n "$ifname" ]]; then
            cat "/sys/class/net/$ifname/address" 2>/dev/null
            return
        fi
    fi
    
    # If bound to DPDK, get from dpdk-devbind output
    # This is a backup - metadata is the primary source
    dpdk-devbind.py --status 2>/dev/null | grep "$pci" | grep -oP "'[0-9a-fA-F:]+'" | tr -d "'" | head -1
}

# Generate configuration JSON
generate_config() {
    local timestamp=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    local instance_type=$(fetch_metadata "/instance-type")
    
    echo "{"
    echo "  \"version\": 2,"
    echo "  \"generated_at\": \"$timestamp\","
    echo "  \"platform\": \"aws-ec2\","
    echo "  \"metadata\": {"
    echo "    \"instance_type\": \"$instance_type\""
    echo "  },"
    
    # Generate dpdk_cores array from low-latency.conf if available
    # Otherwise fallback to all cores except 0
    local ll_config="/etc/dpdk/low-latency.conf"
    local dpdk_cpus=""
    
    if [[ -f "$ll_config" ]]; then
        source "$ll_config"
        dpdk_cpus="$DPDK_CPUS"
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
    local devices_to_bind="${DPDK_DEVICES:-}"
    
    for mac_with_slash in $macs; do
        local mac="${mac_with_slash%/}"  # Remove trailing slash
        
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
        
        # Get interface name from sysfs
        local ifname=""
        local pci=""
        local is_dpdk_bound="false"
        
        # First try to find in sysfs (kernel-bound devices)
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
        
        # If not found in sysfs, device might already be bound to DPDK
        # Use dpdk-devbind to get status and find by checking PCI device properties
        if [[ -z "$pci" ]]; then
            # Get all DPDK-bound devices
            local dpdk_devices=$(dpdk-devbind.py --status 2>/dev/null | grep "drv=vfio-pci" | awk '{print $1}')
            
            for test_pci in $dpdk_devices; do
                # For DPDK-bound devices, we can't get MAC from sysfs
                # But we can use the device-number from metadata to correlate
                # AWS ENA devices on EC2 have predictable PCI slot assignments
                
                # Read the slot from PCI address (e.g., 0000:28:00.0 -> slot 28)
                local slot=$(echo "$test_pci" | cut -d: -f2)
                local slot_dec=$((16#$slot))  # Convert hex to decimal
                
                # ENA device numbers map roughly to PCI slots
                # This is EC2-specific heuristic
                if [[ $((slot_dec - 39)) -eq $device_number ]] || \
                   [[ $((slot_dec - 40)) -eq $device_number ]]; then
                    pci="$test_pci"
                    ifname="enp${slot_dec}s0"  # Reconstruct original name
                    is_dpdk_bound="true"
                    break
                fi
            done
        fi
        
        # Skip if no PCI address found
        [[ -z "$pci" ]] && continue
        
        # Determine role: check ACTUAL current binding status
        local role="kernel"
        if dpdk-devbind.py --status 2>/dev/null | grep "$pci" | grep -q "drv=vfio-pci\|drv=igb_uio"; then
            role="dpdk"
        fi
        # Note: We only report actual state, not intended state
        
        # Calculate gateway (usually .1 of the subnet)
        local gateway_v4=""
        if [[ -n "$subnet_cidr" ]]; then
            local subnet_base="${subnet_cidr%/*}"
            local octets=(${subnet_base//./ })
            gateway_v4="${octets[0]}.${octets[1]}.${octets[2]}.1"
        fi
        
        # Calculate IPv6 gateway (usually ::1 of the subnet)
        local gateway_v6=""
        local prefix_v6="128"
        if [[ -n "$subnet_cidr_v6" ]]; then
            # Extract prefix from IPv6 CIDR (e.g., 2406:da18:e99:5d00::/56 -> 56)
            prefix_v6="${subnet_cidr_v6#*/}"
            [[ -z "$prefix_v6" ]] && prefix_v6="64"
            # AWS VPC IPv6 gateway is typically the first address of the subnet
            local subnet_base_v6="${subnet_cidr_v6%/*}"
            # For /56 or /64 prefixes, gateway is usually ::1
            gateway_v6="${subnet_base_v6%::*}::1"
        fi
        
        # Note: core_affinity removed in version 2, use dpdk_cores instead
        
        # Output JSON
        if [[ "$first" != "true" ]]; then
            echo ","
        fi
        first=false
        
        echo "    {"
        echo "      \"pci_address\": \"$pci\","
        echo "      \"mac\": \"$mac\","
        
        # Build addresses array (IPv4 + IPv6)
        # IMPORTANT: Put IPs with EIP associations FIRST so DPDK uses them for external connectivity
        echo "      \"addresses\": ["
        local addr_first=true
        
        # Get EIP associations for this interface
        local eip_associations=$(fetch_metadata "/network/interfaces/macs/$mac/ipv4-associations/" 2>/dev/null || echo "")
        local ips_with_eip=""
        local ips_without_eip=""
        
        # Categorize IPs by EIP association
        for ip in $local_ipv4s; do
            local has_eip=false
            for eip_with_slash in $eip_associations; do
                local eip="${eip_with_slash%/}"
                # Check if this EIP is associated with this private IP
                local associated_private=$(fetch_metadata "/network/interfaces/macs/$mac/ipv4-associations/$eip" 2>/dev/null || echo "")
                if [[ "$associated_private" == "$ip" ]]; then
                    has_eip=true
                    break
                fi
            done
            
            if [[ "$has_eip" == "true" ]]; then
                ips_with_eip="$ips_with_eip $ip"
            else
                ips_without_eip="$ips_without_eip $ip"
            fi
        done
        
        # Output ONLY IPs with public IP associations (EIP or auto-assigned)
        # Private IPs without public IP mapping cannot be used for external connectivity
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
