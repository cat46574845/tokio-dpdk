/*
 * DPDK Inline Function Wrappers
 *
 * DPDK uses many static inline functions (e.g., rte_eth_rx_burst) that cannot
 * be directly called via FFI. This file provides non-inline wrapper functions
 * that can be linked from Rust.
 *
 * Compile with: cc -O3 -march=native $(pkg-config --cflags libdpdk) -c dpdk_wrappers.c
 *
 * Generated for DPDK 23.11.0 LTS
 */

#include <rte_eal.h>
#include <rte_ethdev.h>
#include <rte_mbuf.h>
#include <rte_mempool.h>
#include <rte_lcore.h>

/* Wrapper for rte_eth_rx_burst (static inline in rte_ethdev.h) */
uint16_t dpdk_wrap_rte_eth_rx_burst(
    uint16_t port_id,
    uint16_t queue_id,
    struct rte_mbuf **rx_pkts,
    const uint16_t nb_pkts)
{
    return rte_eth_rx_burst(port_id, queue_id, rx_pkts, nb_pkts);
}

/* Wrapper for rte_eth_tx_burst (static inline in rte_ethdev.h) */
uint16_t dpdk_wrap_rte_eth_tx_burst(
    uint16_t port_id,
    uint16_t queue_id,
    struct rte_mbuf **tx_pkts,
    uint16_t nb_pkts)
{
    return rte_eth_tx_burst(port_id, queue_id, tx_pkts, nb_pkts);
}

/* Wrapper for rte_pktmbuf_alloc (static inline in rte_mbuf.h) */
struct rte_mbuf *dpdk_wrap_rte_pktmbuf_alloc(struct rte_mempool *mp)
{
    return rte_pktmbuf_alloc(mp);
}

/* Wrapper for rte_pktmbuf_free (static inline in rte_mbuf.h) */
void dpdk_wrap_rte_pktmbuf_free(struct rte_mbuf *m)
{
    rte_pktmbuf_free(m);
}

/* Wrapper for rte_pktmbuf_mtod (macro in rte_mbuf.h) */
void *dpdk_wrap_rte_pktmbuf_mtod(struct rte_mbuf *m)
{
    return rte_pktmbuf_mtod(m, void *);
}

/* Wrapper for rte_pktmbuf_data_len (static inline in rte_mbuf.h) */
uint16_t dpdk_wrap_rte_pktmbuf_data_len(const struct rte_mbuf *m)
{
    return rte_pktmbuf_data_len(m);
}

/* Wrapper for rte_pktmbuf_pkt_len (static inline in rte_mbuf.h) */
uint32_t dpdk_wrap_rte_pktmbuf_pkt_len(const struct rte_mbuf *m)
{
    return rte_pktmbuf_pkt_len(m);
}

/* Wrapper for rte_pktmbuf_append (static inline in rte_mbuf.h) */
char *dpdk_wrap_rte_pktmbuf_append(struct rte_mbuf *m, uint16_t len)
{
    return rte_pktmbuf_append(m, len);
}

/* Wrapper for rte_pktmbuf_headroom (static inline in rte_mbuf.h) */
uint16_t dpdk_wrap_rte_pktmbuf_headroom(const struct rte_mbuf *m)
{
    return rte_pktmbuf_headroom(m);
}

/* Wrapper for rte_pktmbuf_tailroom (static inline in rte_mbuf.h) */
uint16_t dpdk_wrap_rte_pktmbuf_tailroom(const struct rte_mbuf *m)
{
    return rte_pktmbuf_tailroom(m);
}

/* ============================================================
 * rte_flow wrappers for multi-queue traffic routing
 * ============================================================ */

#include <rte_flow.h>

/**
 * Create a flow rule to direct traffic with a specific destination IP
 * to a particular RX queue.
 *
 * @param port_id     DPDK port ID
 * @param priority    Rule priority (lower = higher priority)
 * @param is_ipv4     1 for IPv4, 0 for IPv6
 * @param dst_ip      Destination IP address bytes (4 for IPv4, 16 for IPv6)
 * @param dst_mask    Destination IP mask bytes (4 for IPv4, 16 for IPv6)
 * @param queue_id    Target RX queue to direct matching traffic
 * @param error       Error information on failure
 * @return            Flow rule handle on success, NULL on failure
 */
struct rte_flow *dpdk_wrap_rte_flow_create_queue_rule(
    uint16_t port_id,
    uint32_t priority,
    int is_ipv4,
    const uint8_t *dst_ip,
    const uint8_t *dst_mask,
    uint16_t queue_id,
    struct rte_flow_error *error)
{
    /* Flow attributes: ingress traffic */
    struct rte_flow_attr attr = {
        .group = 0,
        .priority = priority,
        .ingress = 1,
        .egress = 0,
        .transfer = 0,
    };

    /* Pattern: ETH / IPv4 or IPv6 (dst_addr match) / END */
    struct rte_flow_item pattern[3];
    memset(pattern, 0, sizeof(pattern));

    /* Item 0: Ethernet (match any) */
    struct rte_flow_item_eth eth_spec;
    struct rte_flow_item_eth eth_mask;
    memset(&eth_spec, 0, sizeof(eth_spec));
    memset(&eth_mask, 0, sizeof(eth_mask));
    pattern[0].type = RTE_FLOW_ITEM_TYPE_ETH;
    pattern[0].spec = &eth_spec;
    pattern[0].mask = &eth_mask;

    /* Item 1: IPv4 or IPv6 with dst_addr */
    struct rte_flow_item_ipv4 ipv4_spec;
    struct rte_flow_item_ipv4 ipv4_mask;
    struct rte_flow_item_ipv6 ipv6_spec;
    struct rte_flow_item_ipv6 ipv6_mask;

    if (is_ipv4) {
        memset(&ipv4_spec, 0, sizeof(ipv4_spec));
        memset(&ipv4_mask, 0, sizeof(ipv4_mask));
        memcpy(&ipv4_spec.hdr.dst_addr, dst_ip, 4);
        memcpy(&ipv4_mask.hdr.dst_addr, dst_mask, 4);
        pattern[1].type = RTE_FLOW_ITEM_TYPE_IPV4;
        pattern[1].spec = &ipv4_spec;
        pattern[1].mask = &ipv4_mask;
    } else {
        memset(&ipv6_spec, 0, sizeof(ipv6_spec));
        memset(&ipv6_mask, 0, sizeof(ipv6_mask));
        memcpy(ipv6_spec.hdr.dst_addr, dst_ip, 16);
        memcpy(ipv6_mask.hdr.dst_addr, dst_mask, 16);
        pattern[1].type = RTE_FLOW_ITEM_TYPE_IPV6;
        pattern[1].spec = &ipv6_spec;
        pattern[1].mask = &ipv6_mask;
    }

    /* Item 2: End of pattern */
    pattern[2].type = RTE_FLOW_ITEM_TYPE_END;

    /* Action: Direct to specific queue */
    struct rte_flow_action_queue queue_action = {
        .index = queue_id,
    };

    struct rte_flow_action actions[2];
    memset(actions, 0, sizeof(actions));
    actions[0].type = RTE_FLOW_ACTION_TYPE_QUEUE;
    actions[0].conf = &queue_action;
    actions[1].type = RTE_FLOW_ACTION_TYPE_END;

    /* Create the flow rule */
    return rte_flow_create(port_id, &attr, pattern, actions, error);
}

/**
 * Destroy a flow rule.
 *
 * @param port_id   DPDK port ID
 * @param flow      Flow rule handle to destroy
 * @param error     Error information on failure
 * @return          0 on success, negative on failure
 */
int dpdk_wrap_rte_flow_destroy(uint16_t port_id, struct rte_flow *flow,
                               struct rte_flow_error *error)
{
    return rte_flow_destroy(port_id, flow, error);
}

/**
 * Flush all flow rules on a port.
 *
 * @param port_id   DPDK port ID
 * @param error     Error information on failure
 * @return          0 on success, negative on failure
 */
int dpdk_wrap_rte_flow_flush(uint16_t port_id, struct rte_flow_error *error)
{
    return rte_flow_flush(port_id, error);
}

/**
 * Validate a flow rule without creating it.
 *
 * This is used to check if rte_flow is supported by the driver without
 * actually creating a rule.
 *
 * @param port_id     DPDK port ID
 * @param priority    Rule priority (lower = higher priority)
 * @param is_ipv4     1 for IPv4, 0 for IPv6
 * @param dst_ip      Destination IP address bytes (4 for IPv4, 16 for IPv6)
 * @param dst_mask    Destination IP mask bytes (4 for IPv4, 16 for IPv6)
 * @param queue_id    Target RX queue (not actually used, just for validation)
 * @param error       Error information on failure
 * @return            0 on success (rule is valid), negative on failure
 */
int dpdk_wrap_rte_flow_validate_queue_rule(
    uint16_t port_id,
    uint32_t priority,
    int is_ipv4,
    const uint8_t *dst_ip,
    const uint8_t *dst_mask,
    uint16_t queue_id,
    struct rte_flow_error *error)
{
    /* Flow attributes: ingress traffic */
    struct rte_flow_attr attr = {
        .group = 0,
        .priority = priority,
        .ingress = 1,
        .egress = 0,
        .transfer = 0,
    };

    /* Pattern: ETH / IPv4 or IPv6 (dst_addr match) / END */
    struct rte_flow_item pattern[3];
    memset(pattern, 0, sizeof(pattern));

    /* Item 0: Ethernet (match any) */
    struct rte_flow_item_eth eth_spec;
    struct rte_flow_item_eth eth_mask;
    memset(&eth_spec, 0, sizeof(eth_spec));
    memset(&eth_mask, 0, sizeof(eth_mask));
    pattern[0].type = RTE_FLOW_ITEM_TYPE_ETH;
    pattern[0].spec = &eth_spec;
    pattern[0].mask = &eth_mask;

    /* Item 1: IPv4 or IPv6 with dst_addr */
    struct rte_flow_item_ipv4 ipv4_spec;
    struct rte_flow_item_ipv4 ipv4_mask;
    struct rte_flow_item_ipv6 ipv6_spec;
    struct rte_flow_item_ipv6 ipv6_mask;

    if (is_ipv4) {
        memset(&ipv4_spec, 0, sizeof(ipv4_spec));
        memset(&ipv4_mask, 0, sizeof(ipv4_mask));
        memcpy(&ipv4_spec.hdr.dst_addr, dst_ip, 4);
        memcpy(&ipv4_mask.hdr.dst_addr, dst_mask, 4);
        pattern[1].type = RTE_FLOW_ITEM_TYPE_IPV4;
        pattern[1].spec = &ipv4_spec;
        pattern[1].mask = &ipv4_mask;
    } else {
        memset(&ipv6_spec, 0, sizeof(ipv6_spec));
        memset(&ipv6_mask, 0, sizeof(ipv6_mask));
        memcpy(ipv6_spec.hdr.dst_addr, dst_ip, 16);
        memcpy(ipv6_mask.hdr.dst_addr, dst_mask, 16);
        pattern[1].type = RTE_FLOW_ITEM_TYPE_IPV6;
        pattern[1].spec = &ipv6_spec;
        pattern[1].mask = &ipv6_mask;
    }

    /* Item 2: End of pattern */
    pattern[2].type = RTE_FLOW_ITEM_TYPE_END;

    /* Action: Direct to specific queue */
    struct rte_flow_action_queue queue_action = {
        .index = queue_id,
    };

    struct rte_flow_action actions[2];
    memset(actions, 0, sizeof(actions));
    actions[0].type = RTE_FLOW_ACTION_TYPE_QUEUE;
    actions[0].conf = &queue_action;
    actions[1].type = RTE_FLOW_ACTION_TYPE_END;

    /* Validate the flow rule (do not create) */
    return rte_flow_validate(port_id, &attr, pattern, actions, error);
}
