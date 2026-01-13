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
//!
//! ```sh
//! # Linux:
//! # May need to set LIBCLANG_PATH if bindgen cannot find libclang
//! LIBCLANG_PATH=/usr/lib/llvm-18/lib ./tools/generate_dpdk_bindings.sh
//!
//! # Windows:
//! tools\generate_dpdk_bindings_windows.bat
//! ```
//!
//! The script generates bindings for both DPDK library functions and our
//! custom wrapper functions defined in `dpdk_wrappers.c`. The wrapper
//! declarations are embedded in the script, so adding new wrappers requires
//! updating both `dpdk_wrappers.c` and `generate_dpdk_bindings.sh`.

// Suppress warnings from auto-generated code
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::all)]

// Linux bindings (pre-generated from DPDK 23.11.0 LTS)
#[cfg(target_os = "linux")]
#[allow(warnings)]
mod bindings_linux;

#[cfg(target_os = "linux")]
pub(super) use bindings_linux::*;

// Windows bindings (pre-generated from DPDK 23.11.0 LTS)
#[cfg(target_os = "windows")]
#[allow(warnings)]
mod bindings_windows;

#[cfg(target_os = "windows")]
pub(super) use bindings_windows::*;
