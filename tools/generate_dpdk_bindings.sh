#!/bin/bash
# DPDK FFI Binding Generator for tokio-dpdk
# Generates pre-built bindings for Linux (checked into repo)

set -e

OUT_DIR="/tmp/dpdk_bindings"
mkdir -p "$OUT_DIR"

# Get DPDK include paths from pkg-config
DPDK_CFLAGS=$(pkg-config --cflags libdpdk)
echo "DPDK CFLAGS: $DPDK_CFLAGS"

# Create wrapper header
cat > "$OUT_DIR/dpdk_wrapper.h" << 'EOF'
/* Auto-generated header for bindgen */
#include <rte_eal.h>
#include <rte_ethdev.h>
#include <rte_mbuf.h>
#include <rte_mempool.h>
#include <rte_lcore.h>
#include <rte_flow.h>

/* Wrapper function declarations for inline functions */
uint16_t dpdk_wrap_rte_eth_rx_burst(uint16_t port_id, uint16_t queue_id, struct rte_mbuf **rx_pkts, const uint16_t nb_pkts);
uint16_t dpdk_wrap_rte_eth_tx_burst(uint16_t port_id, uint16_t queue_id, struct rte_mbuf **tx_pkts, uint16_t nb_pkts);
struct rte_mbuf *dpdk_wrap_rte_pktmbuf_alloc(struct rte_mempool *mp);
void dpdk_wrap_rte_pktmbuf_free(struct rte_mbuf *m);
void *dpdk_wrap_rte_pktmbuf_mtod(struct rte_mbuf *m);
uint16_t dpdk_wrap_rte_pktmbuf_data_len(const struct rte_mbuf *m);
uint32_t dpdk_wrap_rte_pktmbuf_pkt_len(const struct rte_mbuf *m);
char *dpdk_wrap_rte_pktmbuf_append(struct rte_mbuf *m, uint16_t len);
uint16_t dpdk_wrap_rte_pktmbuf_headroom(const struct rte_mbuf *m);
uint16_t dpdk_wrap_rte_pktmbuf_tailroom(const struct rte_mbuf *m);

/* rte_flow wrapper for creating queue-based flow rules */
struct rte_flow *dpdk_wrap_rte_flow_create_queue_rule(
    uint16_t port_id,
    uint32_t priority,
    int is_ipv4,
    const uint8_t *dst_ip,
    const uint8_t *dst_mask,
    uint16_t queue_id,
    struct rte_flow_error *error);
int dpdk_wrap_rte_flow_validate_queue_rule(
    uint16_t port_id,
    uint32_t priority,
    int is_ipv4,
    const uint8_t *dst_ip,
    const uint8_t *dst_mask,
    uint16_t queue_id,
    struct rte_flow_error *error);
int dpdk_wrap_rte_flow_destroy(uint16_t port_id, struct rte_flow *flow, struct rte_flow_error *error);
int dpdk_wrap_rte_flow_flush(uint16_t port_id, struct rte_flow_error *error);
EOF

echo "Generated wrapper header at $OUT_DIR/dpdk_wrapper.h"

# Generate bindings using bindgen
# Use -- separator to pass clang args
bindgen "$OUT_DIR/dpdk_wrapper.h" \
    --allowlist-function "rte_eal_init" \
    --allowlist-function "rte_eal_cleanup" \
    --allowlist-function "rte_eth_dev_count_avail" \
    --allowlist-function "rte_eth_dev_configure" \
    --allowlist-function "rte_eth_dev_start" \
    --allowlist-function "rte_eth_dev_stop" \
    --allowlist-function "rte_eth_dev_close" \
    --allowlist-function "rte_eth_rx_queue_setup" \
    --allowlist-function "rte_eth_tx_queue_setup" \
    --allowlist-function "rte_eth_promiscuous_enable" \
    --allowlist-function "rte_eth_allmulticast_enable" \
    --allowlist-function "rte_eth_dev_socket_id" \
    --allowlist-function "rte_eth_dev_info_get" \
    --allowlist-function "rte_eth_macaddr_get" \
    --allowlist-function "rte_pktmbuf_pool_create" \
    --allowlist-function "rte_mempool_free" \
    --allowlist-function "rte_socket_id" \
    --allowlist-function "rte_lcore_id" \
    --allowlist-function "rte_flow_create" \
    --allowlist-function "rte_flow_destroy" \
    --allowlist-function "rte_flow_flush" \
    --allowlist-function "rte_flow_validate" \
    --allowlist-function "dpdk_wrap_.*" \
    --allowlist-type "rte_mbuf" \
    --allowlist-type "rte_mempool" \
    --allowlist-type "rte_eth_conf" \
    --allowlist-type "rte_eth_dev_info" \
    --allowlist-type "rte_eth_rxconf" \
    --allowlist-type "rte_eth_txconf" \
    --allowlist-type "rte_ether_addr" \
    --allowlist-type "rte_flow" \
    --allowlist-type "rte_flow_attr" \
    --allowlist-type "rte_flow_item" \
    --allowlist-type "rte_flow_item_type" \
    --allowlist-type "rte_flow_item_ipv4" \
    --allowlist-type "rte_flow_item_ipv6" \
    --allowlist-type "rte_flow_item_eth" \
    --allowlist-type "rte_flow_action" \
    --allowlist-type "rte_flow_action_type" \
    --allowlist-type "rte_flow_action_queue" \
    --allowlist-type "rte_flow_error" \
    --use-core \
    --output "$OUT_DIR/bindings_linux.rs" \
    -- $DPDK_CFLAGS

echo "Generated bindings at $OUT_DIR/bindings_linux.rs"
echo "Lines: $(wc -l < $OUT_DIR/bindings_linux.rs)"

# Copy to tokio-dpdk (default to relative path from script location)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOKIO_FFI_DIR="${TOKIO_FFI_DIR:-$SCRIPT_DIR/../tokio/src/runtime/scheduler/dpdk/ffi}"
cp "$OUT_DIR/bindings_linux.rs" "$TOKIO_FFI_DIR/bindings_linux.rs"

echo ""
echo "=== Bindings generated and copied successfully ==="
echo "Output: $TOKIO_FFI_DIR/bindings_linux.rs"
