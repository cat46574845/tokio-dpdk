//! DPDK FFI bindings.
//!
//! This module provides FFI bindings to DPDK (Data Plane Development Kit).
//! The bindings are pre-generated from DPDK 23.11.0 LTS and checked into the repository
//! to avoid requiring clang/bindgen at build time.
//!
//! # Platform Support
//!
//! - **Linux**: Full support with pre-generated bindings from DPDK 23.11.0 LTS
//! - **Windows**: Full support with pre-generated bindings from DPDK 23.11.0 LTS
//!
//! # Build Requirements
//!
//! At runtime, DPDK libraries must be available:
//! - Linux: Install DPDK (libdpdk) via package manager or from source
//! - Windows: Install DPDK built with Clang+MSVC linker
//!
//! # Inline Function Wrappers
//!
//! DPDK uses many static inline functions (e.g., `rte_eth_rx_burst`).
//! These cannot be directly called via FFI, so we provide C wrapper
//! functions that must be compiled and linked at build time.
//! See `dpdk_wrappers.c` for the wrapper implementations.
//!
//! # Regenerating Bindings
//!
//! To regenerate bindings (requires DPDK and bindgen installed):
//! ```sh
//! # Linux (WSL):
//! ./tools/generate_dpdk_bindings.sh
//!
//! # Windows:
//! tools\generate_dpdk_bindings_windows.bat
//! ```

// Suppress warnings from auto-generated code
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::all)]

// Linux bindings (pre-generated from DPDK 23.11.0 LTS)
#[cfg(all(feature = "dpdk", target_os = "linux"))]
mod bindings_linux;

#[cfg(all(feature = "dpdk", target_os = "linux"))]
pub use bindings_linux::*;

// Windows bindings (pre-generated from DPDK 23.11.0 LTS)
#[cfg(all(feature = "dpdk", target_os = "windows"))]
mod bindings_windows;

#[cfg(all(feature = "dpdk", target_os = "windows"))]
pub use bindings_windows::*;

// When dpdk feature is not enabled, provide empty module
#[cfg(not(feature = "dpdk"))]
mod empty {
    // No DPDK support when feature is disabled
}
