# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Working Guidelines

**Claude Code's Responsibility:** Do the work yourself. Do not delegate to the user.

The user's role is limited to:
- Making technical decisions when there are multiple valid approaches
- Reviewing and approving results

Claude Code should independently handle everything else:
- Researching information (web searches, documentation)
- Investigating code issues and debugging
- Running tests and validating solutions
- Setting up and configuring environments
- Tracing problems through the codebase

Do not ask the user to do research, check documentation, run commands, or investigate issues themselves. Use available tools to complete all work and report back with findings and solutions.

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

**IMPORTANT:** Always use `cargo-fast` instead of `cargo` for all build, check, and test commands. This applies to `build`, `check`, `test`, `clippy`, and all other cargo subcommands.

**CRITICAL: NEVER use `tail`, `head`, or any other command to truncate build/test/command output. ALWAYS show the FULL, COMPLETE, UNTRUNCATED output. Truncated output hides errors and wastes debugging time. This is a HARD requirement with NO exceptions.**

```bash
# Standard build (requires DPDK)
cargo-fast build

# Build specific package
cargo-fast build -p tokio

# Build will compile DPDK C wrappers (build.rs)
# On Windows: uses hardcoded paths to DPDK installation
# On Linux: uses pkg-config to find DPDK
```

### Running Tests

DPDK tests require environment variables and sudo isolation:

```bash
# Run all tests (recommended)
source .env && export DPDK_TEST_PORT
SERIAL_ISOLATION_SUDO=1 cargo-fast test --manifest-path tokio/Cargo.toml --features "full,test-util"

# Run specific DPDK test
source .env && export DPDK_TEST_PORT
SERIAL_ISOLATION_SUDO=1 cargo-fast test --manifest-path tokio/Cargo.toml --test tcp_dpdk --features full

# Run single test function
source .env && export DPDK_TEST_PORT
SERIAL_ISOLATION_SUDO=1 cargo-fast test --manifest-path tokio/Cargo.toml --test tcp_dpdk --features full -- test_name
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
- **env_config.rs** - Environment configuration loading from env.json

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
2. `builder.dpdk_pci_addresses(...)` optionally specifies devices
3. `builder.worker_threads(N)` optionally specifies worker count
4. `builder.build()` triggers:
   - env.json loading and validation
   - AllocationPlan creation
   - DPDK EAL initialization
   - Memory pool creation
   - Port initialization
   - Worker creation with CPU affinity
   - IO thread spawn

### Key Files

- **tokio/src/runtime/builder.rs:142** - DpdkBuilder field added to Builder
- **tokio/src/runtime/scheduler/mod.rs** - Scheduler enum with Dpdk variant
- **tokio/src/net/tcp_dpdk/** - TcpDpdkStream and TcpDpdkListener APIs
- **tokio/build.rs** - Compiles DPDK C wrappers (dpdk_wrappers.c)
- **tokio/tests/tcp_dpdk.rs** - Integration tests for DPDK networking
- **tokio/src/runtime/scheduler/dpdk/env_config.rs** - env.json loading and environment configuration
- **tokio/src/runtime/scheduler/dpdk/allocation.rs** - AllocationPlan for device/worker/core assignment

## API Usage

### Creating DPDK Runtime

The runtime auto-discovers all NIC devices, IP addresses, MAC, gateway, and core assignments from `/etc/dpdk/env.json` (generated by `setup.sh`). You do NOT need to specify PCI addresses — just build and go.

```rust
// Standard usage - auto-detects everything from env.json
let rt = tokio::runtime::Builder::new_dpdk()
    .enable_all()
    .build()?;

// With explicit worker count
let rt = Builder::new_dpdk()
    .worker_threads(1)
    .enable_all()
    .build()?;
```

### Builder Methods

- `worker_threads(N)` - Set number of worker threads (replaces the old `dpdk_num_workers()`)
- `dpdk_eal_arg(&str)` - Pass additional EAL arguments to DPDK initialization
- `dpdk_mempool_size(usize)` - Configure the DPDK memory pool size
- `dpdk_cache_size(usize)` - Configure the DPDK mempool cache size
- `dpdk_queue_descriptors(u16)` - Configure the number of RX/TX queue descriptors
- `dpdk_pci_addresses(&[&str])` - **Rarely needed.** Filter to specific PCI devices. Omit to auto-use all DPDK devices from env.json

### Using TcpDpdkStream

```rust
use tokio::net::TcpDpdkStream;

rt.block_on(async {
    // Connect (uses DPDK userspace networking)
    let mut stream = TcpDpdkStream::connect("192.168.1.100:8080").await?;

    // Read/write
    stream.write_all(b"hello").await?;
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
});
```

**Note:** `TcpDpdkStream` is separate from `tokio::net::TcpStream`. Using `TcpStream` in a DPDK runtime will still use kernel networking.

## Dependencies

- **smoltcp** (0.11) - Pure Rust TCP/IP stack (no std, alloc-only)
- **DPDK** (23.11 or 24.11) - C library for fast packet processing
- Core tokio dependencies (bytes, mio, parking_lot, etc.)

## Test Writing Rules

- **Non-tokio-native tests must fail, not skip.** All DPDK tests (and any other tests that are not part of upstream Tokio) must `panic!` when the required environment is not available, rather than printing "SKIPPED" and returning successfully. A silent skip reports as a passing test, which hides missing infrastructure. If a test cannot run, it must fail loudly so the problem is visible.

## Platform Notes

- **Windows**: Uses pre-built DPDK binaries, hardcoded paths in build.rs
- **Linux**: Primary DPDK platform, uses pkg-config for DPDK discovery
- **WASM**: DPDK scheduler not available (not target_family = "wasm")
