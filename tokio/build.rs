//! Build script for tokio with DPDK support.
//!
//! This compiles the DPDK C wrapper functions required for FFI calls to
//! DPDK inline functions.

fn main() {
    // Only compile DPDK wrappers on Linux (DPDK primary platform)
    // Windows support is via pre-built binaries
    #[cfg(target_os = "linux")]
    compile_dpdk_wrappers_linux();

    #[cfg(target_os = "windows")]
    compile_dpdk_wrappers_windows();

    // Tell cargo to rerun if the wrapper source changes
    println!("cargo:rerun-if-changed=src/runtime/scheduler/dpdk/ffi/dpdk_wrappers.c");
}

#[cfg(target_os = "linux")]
fn compile_dpdk_wrappers_linux() {
    // Check if pkg-config can find DPDK
    let dpdk_cflags = std::process::Command::new("pkg-config")
        .args(["--cflags", "libdpdk"])
        .output();

    match dpdk_cflags {
        Ok(output) if output.status.success() => {
            let cflags = String::from_utf8_lossy(&output.stdout);
            let cflags: Vec<&str> = cflags.split_whitespace().collect();

            // Compile the wrapper library
            let mut build = cc::Build::new();
            build
                .file("src/runtime/scheduler/dpdk/ffi/dpdk_wrappers.c")
                .opt_level(3)
                // Enable required CPU features for DPDK intrinsics
                .flag("-mssse3")
                .flag("-msse4.1")
                .flag("-msse4.2");

            // Add DPDK include flags and other compiler flags
            for flag in &cflags {
                if flag.starts_with("-I") {
                    build.include(&flag[2..]);
                } else if flag.starts_with("-m") {
                    // Pass through architecture-specific flags
                    build.flag(flag);
                }
            }

            build.compile("dpdk_wrappers");

            // Link against DPDK libraries
            let dpdk_libs = std::process::Command::new("pkg-config")
                .args(["--libs", "libdpdk"])
                .output()
                .expect("Failed to get DPDK libs");

            let libs = String::from_utf8_lossy(&dpdk_libs.stdout);
            for lib in libs.split_whitespace() {
                if lib.starts_with("-l") {
                    println!("cargo:rustc-link-lib={}", &lib[2..]);
                } else if lib.starts_with("-L") {
                    println!("cargo:rustc-link-search=native={}", &lib[2..]);
                }
            }
        }
        _ => {
            // DPDK not found via pkg-config - skip compilation
            // This allows building on systems without DPDK installed
            // Runtime will fail if DPDK functions are actually called
            println!("cargo:warning=DPDK not found via pkg-config, DPDK wrappers not compiled");
            println!("cargo:warning=Install DPDK and ensure pkg-config can find libdpdk");
        }
    }
}

#[cfg(target_os = "windows")]
fn compile_dpdk_wrappers_windows() {
    // Windows DPDK installation path
    // DPDK 23.11 is pre-built at this location
    let dpdk_build_dir = "F:\\dpdk\\dpdk-23.11\\build";
    let dpdk_lib_dir = format!("{}\\lib", dpdk_build_dir);

    // Also need the source headers
    let dpdk_src_dir = "F:\\dpdk\\dpdk-23.11";

    // Check if DPDK is available
    let lib_path = std::path::Path::new(&dpdk_lib_dir);
    if !lib_path.exists() {
        println!("cargo:warning=DPDK not found at {}", dpdk_build_dir);
        println!("cargo:warning=Windows DPDK wrappers not compiled");
        return;
    }

    // Compile the wrapper library using clang (MSVC can't handle some DPDK headers)
    let wrapper_src = "src/runtime/scheduler/dpdk/ffi/dpdk_wrappers.c";
    if std::path::Path::new(wrapper_src).exists() {
        let mut build = cc::Build::new();

        // Windows SDK paths for system headers (time.h, etc.)
        // Try to find Windows SDK
        let sdk_root = "C:\\Program Files (x86)\\Windows Kits\\10";
        let sdk_version = "10.0.22621.0"; // Latest installed version

        // Use clang compiler (DPDK headers require clang on Windows)
        build.compiler("clang");

        build
            .file(wrapper_src)
            .opt_level(3)
            // Enable required CPU features for DPDK (clang flags)
            .flag("-mssse3")
            .flag("-msse4.1")
            .flag("-msse4.2")
            // Target Windows MSVC ABI
            .flag("--target=x86_64-pc-windows-msvc")
            .include(format!("{}\\Include\\{}\\ucrt", sdk_root, sdk_version))
            .include(format!("{}\\Include\\{}\\um", sdk_root, sdk_version))
            .include(format!("{}\\Include\\{}\\shared", sdk_root, sdk_version))
            // DPDK headers from source
            .include(format!("{}\\lib\\eal\\include", dpdk_src_dir))
            .include(format!("{}\\lib\\eal\\windows\\include", dpdk_src_dir))
            .include(format!("{}\\lib\\eal\\x86\\include", dpdk_src_dir))
            .include(format!("{}\\lib\\ethdev", dpdk_src_dir))
            .include(format!("{}\\lib\\mbuf", dpdk_src_dir))
            .include(format!("{}\\lib\\mempool", dpdk_src_dir))
            .include(format!("{}\\lib\\ring", dpdk_src_dir))
            .include(format!("{}\\lib\\net", dpdk_src_dir))
            .include(format!("{}\\lib\\meter", dpdk_src_dir))
            .include(format!("{}\\lib\\log", dpdk_src_dir))
            .include(format!("{}\\lib\\kvargs", dpdk_src_dir))
            .include(format!("{}\\lib\\telemetry", dpdk_src_dir))
            .include(format!("{}\\lib\\hash", dpdk_src_dir))
            .include(format!("{}\\lib\\rcu", dpdk_src_dir))
            .include(format!("{}\\lib\\pci", dpdk_src_dir))
            .include(format!("{}\\config", dpdk_src_dir))
            // Build config from compiled DPDK
            .include(&dpdk_build_dir);

        // Try to compile, but don't fail the build if it doesn't work
        // The pre-built DPDK libraries should provide the symbols we need
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build.try_compile("dpdk_wrappers")
        })) {
            Ok(Ok(())) => {
                println!("cargo:warning=DPDK wrappers compiled successfully");
            }
            Ok(Err(e)) => {
                println!("cargo:warning=DPDK wrappers compilation failed: {}", e);
            }
            Err(_) => {
                println!("cargo:warning=DPDK wrappers compilation panicked");
            }
        }
    }

    // Link against DPDK libraries
    println!("cargo:rustc-link-search=native={}", dpdk_lib_dir);

    // Core DPDK libraries needed for tokio-dpdk
    let dpdk_libs = [
        "rte_eal",
        "rte_ethdev",
        "rte_mbuf",
        "rte_mempool",
        "rte_ring",
        "rte_net",
        "rte_kvargs",
        "rte_log",
        "rte_telemetry",
        "rte_hash",
        "rte_rcu",
        "rte_pci",
        "rte_timer",
        "rte_meter",
    ];

    for lib in &dpdk_libs {
        println!("cargo:rustc-link-lib={}", lib);
    }

    // Also link against Windows system libraries that DPDK needs
    println!("cargo:rustc-link-lib=ws2_32");
    println!("cargo:rustc-link-lib=mincore");
    println!("cargo:rustc-link-lib=dbghelp");
    println!("cargo:rustc-link-lib=setupapi");
}
