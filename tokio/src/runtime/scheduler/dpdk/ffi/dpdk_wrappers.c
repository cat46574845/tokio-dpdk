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
