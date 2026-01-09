# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a fork of Tokio (asynchronous runtime for Rust) that integrates Intel DPDK (Data Plane Development Kit) for high-performance, low-latency networking. The project adds a new DPDK-based scheduler alongside Tokio's existing current-thread and multi-thread schedulers.

**Key characteristics:**
- DPDK scheduler with per-core workers (no work stealing)
- Busy-poll event loop (workers never park)
- smoltcp-based TCP/IP stack over DPDK
- Zero-copy networking via DPDK mbufs
- Dedicated IO thread for standard I/O and timers

## Building and Testing

### Prerequisites

**Windows:**
- DPDK 23.11 or 24.11 pre-built at `F:\dpdk\dpdk-{version}\build`
- Visual Studio 2017+ Developer Command Prompt
- Hugepages configured (see `grant_hugepage_privilege.ps1`)

**Linux:**
- DPDK installed and discoverable via `pkg-config --cflags libdpdk`
- Hugepages configured

### Build Commands

```bash
# Standard build (requires DPDK)
cargo build

# Build specific package
cargo build -p tokio

# Build will compile DPDK C wrappers (build.rs)
# On Windows: uses hardcoded paths to DPDK installation
# On Linux: uses pkg-config to find DPDK
```

### Running Tests

```bash
# Run all tests
cargo test

# Run DPDK-specific tests (requires DPDK environment)
cargo test --test tcp_dpdk

# Run tokio core tests
cargo test -p tokio

# Run single test
cargo test --test tcp_dpdk tcp_dpdk_stream_connect
```

### Common Issues

- **DPDK not found**: Build warnings about missing DPDK are expected if not installed. Runtime will fail only when DPDK functions are called.
- **Hugepages**: DPDK requires hugepages. On Windows, use `grant_hugepage_privilege.ps1` to configure.
- **String initialization warnings**: The cargo_errors.txt shows harmless DPDK header warnings during C wrapper compilation.

## Architecture

### Scheduler System

The codebase contains three scheduler types in `tokio/src/runtime/scheduler/`:
1. **current_thread** - Single-threaded scheduler
2. **multi_thread** - Work-stealing multi-threaded scheduler (original Tokio)
3. **dpdk** - DPDK-based scheduler (new addition)

### DPDK Scheduler Components

Located in `tokio/src/runtime/scheduler/dpdk/`:

- **mod.rs** - Top-level DPDK scheduler struct and initialization
- **config.rs** - DpdkBuilder API for runtime configuration
- **worker.rs** - Per-core worker thread implementation with CPU affinity
- **handle.rs** - Scheduler handle for task spawning
- **dpdk_driver.rs** - Integration between DPDK devices and smoltcp TCP/IP stack
- **device/** - DpdkDevice implementing smoltcp::phy::Device trait
- **ffi/** - FFI bindings to DPDK C API and wrapper functions
- **init.rs** - DPDK EAL initialization, mempool creation, port setup
- **io_thread.rs** - Dedicated thread for standard I/O and timers
- **resolve/** - Automatic device configuration resolution from OS

### Network Stack Integration

**DPDK → smoltcp → Tokio:**
1. DPDK provides raw packet I/O (rx_burst/tx_burst)
2. DpdkDevice implements smoltcp Device trait
3. smoltcp handles TCP/IP protocol stack
4. DpdkDriver integrates smoltcp with Tokio's async runtime
5. TcpDpdkStream exposes Tokio-style async API

### Runtime Creation Flow

When building a DPDK runtime:
1. `Builder::new_dpdk()` creates builder with DpdkBuilder
2. `builder.dpdk_device("eth0")` specifies network devices
3. `builder.build()` triggers:
   - DPDK EAL initialization (rte_eal_init)
   - Memory pool creation for packet buffers
   - Port initialization and configuration
   - Device configuration resolution (IP, gateway, core from OS)
   - Worker creation with CPU affinity
   - IO thread spawn for standard I/O

### Key Files

- **tokio/src/runtime/builder.rs:142** - DpdkBuilder field added to Builder
- **tokio/src/runtime/scheduler/mod.rs** - Scheduler enum with Dpdk variant
- **tokio/src/net/tcp_dpdk/** - TcpDpdkStream and TcpDpdkListener APIs
- **tokio/build.rs** - Compiles DPDK C wrappers (dpdk_wrappers.c)
- **tokio/tests/tcp_dpdk.rs** - Integration tests for DPDK networking

## API Usage

### Creating DPDK Runtime

```rust
// Simple usage
let rt = tokio::runtime::Builder::new_dpdk()
    .dpdk_device("eth0")
    .build()?;

// Multiple devices
let rt = Builder::new_dpdk()
    .dpdk_device("eth0")
    .dpdk_device("eth1")
    .build()?;

// With EAL arguments
let rt = Builder::new_dpdk()
    .dpdk_device("eth0")
    .eal_arg("--no-huge")
    .build()?;
```

### Using TcpDpdkStream

```rust
rt.block_on(async {
    // Connect
    let stream = TcpDpdkStream::connect("192.168.1.100:8080").await?;

    // Read/write
    stream.write_all(b"hello").await?;
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
});
```

## Dependencies

- **smoltcp** (0.11) - Pure Rust TCP/IP stack (no std, alloc-only)
- **DPDK** (23.11 or 24.11) - C library for fast packet processing
- Core tokio dependencies (bytes, mio, parking_lot, etc.)

## Platform Notes

- **Windows**: Uses pre-built DPDK binaries, hardcoded paths in build.rs
- **Linux**: Primary DPDK platform, uses pkg-config for DPDK discovery
- **WASM**: DPDK scheduler not available (not target_family = "wasm")
